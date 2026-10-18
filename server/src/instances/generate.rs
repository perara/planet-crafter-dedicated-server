//! What the host generates in procedural wrecks and world instances: GenerateOnTrigger.Generate
//! (the network objects of a wreck: safes, containers, locked doors, generators) and
//! WorldInstanceScene.InitSceneVariables (a world instance's variation, its wreck generators and
//! its shared random props), from the extractor's wrecks.json and scenes/<planet>.json.
//!
//! ponytail: Tessera, which lays out a wreck's tiles from its seed, is not ported. The extractor
//! has the game lay out seeds 1..=N of every generator, and the server gives its wrecks those
//! seeds (pool_seed). Port Tessera (DeBroglie's WFC on a Sylves cube grid) to take any seed.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use indexmap::IndexMap;
use serde::Deserialize;

use crate::game::Game;
use crate::geometry::{from_euler_y, matrix_mul, matrix_position, matrix_rotation, trs, MathRandom, Matrix};

/// RandomizePropData.RandomData: chance (percent), randomizeRotationY, the prop's group, and
/// the key of the prop's own RandomizePropData list.
type RandomData = (i32, bool, Option<String>, Option<String>);
/// A RandomizePropData: the key of its list and its index there.
type PropId = (String, usize);

#[derive(Deserialize, Default)]
pub struct Wrecks {
    seeds: u32,
    generators: Vec<GeneratorData>,
    props: HashMap<String, Vec<Slot>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeneratorData {
    scale: [f32; 3],
    generator: [f32; 12],
    container: [f32; 12],
    children: Vec<Child>,
    limited_groups: Vec<(Option<String>, i32)>,
    #[serde(default)]
    scene_instance_radius_ratio: f32,
    triggers: Vec<Trigger>,
    /// Seed -> Tessera's tiles: (props key, local position, local rotation).
    layouts: HashMap<String, Vec<(String, [f32; 3], [f32; 4])>>,
}

/// A trigger collider on the generator (a sphere around it).
#[derive(Deserialize)]
struct Trigger {
    #[serde(default)]
    radius: f32,
}

#[derive(Deserialize)]
struct Child {
    props: String,
    ltw: [f32; 12],
}

/// A RandomizePropData, its matrix taken with its root at the origin.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    m: [f32; 12],
    network: bool,
    no_count_limited: bool,
    props: Vec<RandomData>,
    #[serde(default)]
    ignore: Vec<PropId>,
}

/// A world instance scene as HandleDataAfterLoad placed it, before InitSceneVariables.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceScene {
    root: usize,
    variations: Vec<Variation>,
    objects: Vec<Toggle>,
    wreck_entries: Vec<Entry>,
    /// Every RandomizePropData under the scene component: node, position, rotation,
    /// [(chance, group)], indices it makes ignore.
    props: Vec<(usize, [f32; 3], [f32; 4], Vec<(i32, Option<String>)>, Vec<usize>)>,
    pub prevent_building: Option<Shape>,
    /// (parent, activeSelf, name).
    nodes: Vec<(i64, bool, String)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Variation {
    every_time: Vec<usize>,
    /// (min, max exclusive, objects).
    random: Vec<(i32, i32, Vec<usize>)>,
}

/// A WorldVariationObject.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Toggle {
    node: i64,
    whole: bool,
    when_enabled: i64,
    when_disabled: i64,
}

#[derive(Deserialize)]
struct Entry {
    node: usize,
    position: [f32; 3],
    rotation: [f32; 4],
}

/// A box collider in world space.
#[derive(Deserialize, Clone, Debug, serde::Serialize)]
#[serde(try_from = "ShapeRow", into = "ShapeRow")]
pub struct Shape {
    position: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    center: [f32; 3],
    size: [f32; 3],
}

/// Generation.Shape as the scene table writes it: only an active BoxCollider is a Shape.
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ShapeRow {
    #[serde(rename = "type")]
    kind: String,
    active: bool,
    position: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    center: [f32; 3],
    size: [f32; 3],
}

impl TryFrom<ShapeRow> for Shape {
    type Error = String;
    fn try_from(r: ShapeRow) -> Result<Shape, String> {
        if r.kind != "BoxCollider" || !r.active {
            return Err(format!("a {} collider, active {}: only an active BoxCollider is read", r.kind, r.active));
        }
        Ok(Shape { position: r.position, rotation: r.rotation, scale: r.scale, center: r.center, size: r.size })
    }
}

impl From<Shape> for ShapeRow {
    fn from(s: Shape) -> ShapeRow {
        ShapeRow { kind: "BoxCollider".into(), active: true, position: s.position, rotation: s.rotation, scale: s.scale, center: s.center, size: s.size }
    }
}

impl Shape {
    /// Collider.ClosestPoint(p) == p for a box.
    pub fn contains(&self, p: [f32; 3]) -> bool {
        let local = crate::geometry::rotate(crate::geometry::conj(self.rotation), crate::geometry::sub(p, self.position));
        (0..3).all(|i| (local[i] / self.scale[i] - self.center[i]).abs() <= self.size[i] / 2.0)
    }
}

fn affine(a: &[f32; 12]) -> Matrix {
    [[a[0], a[1], a[2], a[3]], [a[4], a[5], a[6], a[7]], [a[8], a[9], a[10], a[11]], [0.0, 0.0, 0.0, 1.0]]
}

impl Wrecks {
    /// wrecks.json, with a layout pool and the generator of every wreck size.
    pub fn parse(text: &str) -> Result<Wrecks, String> {
        let w: Wrecks = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if w.seeds == 0 || w.generators.len() != super::GENERATORS.len() {
            return Err(format!("{} seeds and {} generators, where the game has {}", w.seeds, w.generators.len(), super::GENERATORS.len()));
        }
        Ok(w)
    }
}

/// wrecks.json, loaded at start.
pub fn wrecks(game: &Game) -> Rc<Wrecks> {
    game.scenes.wrecks.clone()
}

/// A world instance scene of the current planet; None for a name the planet has none of.
pub fn instance_scene(game: &Game, name: &str) -> Option<Rc<InstanceScene>> {
    game.scenes.planets.get(game.planet_id())?.instance_scenes.get(name).cloned()
}

/// GenerateOnTrigger's trigger sphere radius, the scene wrecks' (owner 0) scaled down
/// (OnNetworkSpawn's _sceneInstanceRadiusRatio).
pub fn trigger_radius(game: &Game, index: usize, owner: i32) -> f32 {
    let g = &game.scenes.wrecks.generators[index];
    let radius = g.triggers.iter().map(|t| t.radius).fold(0.0, f32::max);
    if owner == 0 && g.scene_instance_radius_ratio > 0.0 { radius * g.scene_instance_radius_ratio } else { radius }
}

/// The seed a wreck gets: one of the extractor's layouts.
pub fn pool_seed(game: &Game, seed: u32) -> u32 {
    let seeds = game.scenes.wrecks.seeds;
    if seed < 1 || seed > seeds { 1 + seed % seeds } else { seed }
}

/// RandomizePropData.RetrieveRandomProp.
fn retrieve<'a>(id: &PropId, props: &'a [RandomData], ignore: &[PropId], random: &mut MathRandom, ignored: &mut HashSet<PropId>) -> Option<&'a RandomData> {
    if ignored.remove(id) {
        return None;
    }
    let num = random.next_int(0, 101);
    let mut sum = 0;
    for d in props {
        if num >= sum && num <= sum + d.0 {
            ignored.extend(ignore.iter().cloned());
            return Some(d);
        }
        sum += d.0;
    }
    None
}

/// GenerateOnTrigger.InstantiateRandomPropsInHierarchy, the server's half: the props picked
/// for the decoration (whose own props are walked next) and the network slots it buffers.
fn walk(w: &Wrecks, key: &str, ltw: &Matrix, parent: &Matrix, random: &mut MathRandom, ignored: &mut HashSet<PropId>, buffer: &mut Vec<(PropId, Matrix)>) {
    let mut picked = Vec::new();
    for (i, slot) in w.props.get(key).into_iter().flatten().enumerate() {
        let id = (key.to_string(), i);
        if slot.network {
            if !ignored.remove(&id) {
                buffer.push((id, matrix_mul(&matrix_mul(parent, ltw), &affine(&slot.m))));
            }
            continue;
        }
        if let Some(d) = retrieve(&id, &slot.props, &slot.ignore, random, ignored) {
            let mut m = affine(&slot.m);
            if d.1 {
                m = matrix_mul(&m, &trs([0.0; 3], from_euler_y(random.next_float() * 360.0), [1.0; 3]));
            }
            picked.push((d.3.clone(), m));
        }
    }
    for (prop, m) in picked {
        if let Some(prop) = prop {
            walk(w, &prop, &matrix_mul(ltw, &m), parent, random, ignored, buffer);
        }
    }
}

/// The private CreateAndInstantiateWorldObject: a generated object, with what its inventory
/// was filled with (GetGeneratedNetworkObjectIds).
fn create(game: &mut Game, group: &str, position: [f32; 3], rotation: [f32; 4]) -> Vec<i32> {
    let Some(group) = game.data.group(group) else { return Vec::new() };
    let (id, _) = game.create_and_instantiate(group, position, rotation, false, true, true);
    let inventory = game.wos[&id].linked_inventory;
    let mut ids = vec![id];
    ids.extend(game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default());
    ids
}

/// GenerateOnTrigger.Generate on the server: the network objects of a generator's wreck, from
/// its seed's layout. Returns the generated world objects; none without the seed's layout.
pub fn generate(game: &mut Game, index: usize, seed: u32, position: [f32; 3], rotation: [f32; 4]) -> Vec<i32> {
    let w = wrecks(game);
    let Some(data) = w.generators.get(index) else { return Vec::new() };
    let Some(layout) = data.layouts.get(&seed.to_string()) else { return Vec::new() };
    let root = trs(position, rotation, data.scale);
    let mut ignored = std::mem::take(&mut game.ext.planets.instances.ignored);
    // The generator's own children are its instance's (fresh flags); tiles and props are prefabs.
    ignored.retain(|(key, _)| data.children.iter().all(|c| c.props != *key));
    let mut buffer = Vec::new();
    // GenerateRandomPropsFromRoot on the generator's other children (its container stays
    // empty on a server), then the tiles, each from the seed.
    let mut random = MathRandom::new(seed);
    let container = matrix_mul(&root, &affine(&data.container));
    for child in &data.children {
        walk(&w, &child.props, &affine(&child.ltw), &container, &mut random, &mut ignored, &mut buffer);
    }
    let mut random = MathRandom::new(seed);
    let parent = matrix_mul(&root, &affine(&data.generator));
    for (tile, p, q) in layout {
        walk(&w, tile, &trs(*p, *q, [1.0; 3]), &parent, &mut random, &mut ignored, &mut buffer);
    }
    let mut random = MathRandom::new(seed.wrapping_add(1));
    let limit = |group: &str| data.limited_groups.iter().find(|(id, _)| id.as_deref() == Some(group)).map(|(_, n)| *n);
    let mut limited: IndexMap<String, Vec<(bool, Matrix)>> = IndexMap::new();
    let mut made = Vec::new();
    let mut place = |game: &mut Game, group: &str, rotate: bool, ltw: &Matrix, random: &mut MathRandom| {
        let yaw = if rotate { random.next_float() * 360.0 } else { 0.0 };
        let m = matrix_mul(ltw, &trs([0.0; 3], from_euler_y(yaw), [1.0; 3]));
        made.extend(create(game, group, matrix_position(&m), matrix_rotation(&m)));
    };
    for (id, m) in buffer {
        let slot = &w.props[&id.0][id.1];
        let Some((_, rotate, Some(group), _)) = retrieve(&id, &slot.props, &slot.ignore, &mut random, &mut ignored) else { continue };
        if !slot.no_count_limited && limit(group).is_some() {
            limited.entry(group.clone()).or_default().push((*rotate, m));
            continue;
        }
        place(game, group, *rotate, &m, &mut random);
    }
    for (group, mut list) in limited {
        let mut max = limit(&group).unwrap_or(0);
        while max > 0 && !list.is_empty() {
            let (rotate, m) = list.remove(random.next_int(0, list.len() as i32) as usize);
            place(game, &group, rotate, &m, &mut random);
            max -= 1;
        }
    }
    game.ext.planets.instances.ignored = ignored;
    made
}

/// activeSelf of a node and its parents, up to `top` (activeInHierarchy: none).
fn on(scene: &InstanceScene, active: &[bool], mut node: i64, top: Option<usize>) -> bool {
    while node >= 0 {
        if !active[node as usize] {
            return false;
        }
        if Some(node as usize) == top {
            break;
        }
        node = scene.nodes[node as usize].0;
    }
    true
}

/// WorldInstanceScene.InitSceneVariables on the server, for an instance opened now (not loaded
/// from the save): the variation from the instance's seed, a wreck generator per active wreck
/// entry, then the scene's random props (GenerateExternalProps), made by its first generator.
pub fn init_scene_variables(game: &mut Game, seed: i32, difficulty: i32, scene: &str) {
    let Some(scene) = instance_scene(game, scene) else { return };
    let mut random = MathRandom::new(seed as u32);
    let mut active: Vec<bool> = scene.nodes.iter().map(|n| n.1).collect();
    let loaded = active.clone();
    let mut enabled = vec![false; scene.objects.len()];
    let mut scripted = vec![false; scene.objects.len()];
    // WorldVariationObject.SetWorldRandomObjectStatus.
    let set = |active: &mut Vec<bool>, enabled: &mut Vec<bool>, o: usize, on: bool| {
        enabled[o] = on;
        let t = &scene.objects[o];
        for (node, value) in [(if t.whole { t.node } else { t.when_enabled }, on), (if t.whole { -1 } else { t.when_disabled }, !on)] {
            if node >= 0 {
                active[node as usize] = value;
            }
        }
    };
    if !scene.variations.is_empty() {
        let v = &scene.variations[random.next_int(0, scene.variations.len() as i32) as usize];
        for o in &v.every_time {
            scripted[*o] = true;
            set(&mut active, &mut enabled, *o, true);
        }
        // RandomizeObjectApparition.
        for (min, max, list) in &v.random {
            for o in list {
                scripted[*o] = true;
                set(&mut active, &mut enabled, *o, false);
            }
            let count = random.next_int(*min, *max);
            let mut n = 0;
            for _ in 0..2000 {
                if n >= count || list.is_empty() {
                    break;
                }
                let o = list[random.next_int(0, list.len() as i32) as usize];
                if !enabled[o] {
                    set(&mut active, &mut enabled, o, true);
                    n += 1;
                }
            }
        }
    }
    // WorldVariationObject.Awake hides one no variation set when it first becomes active.
    let mut woke = true;
    while woke {
        woke = false;
        for (o, t) in scene.objects.iter().enumerate() {
            if !scripted[o] && on(&scene, &active, t.node, None) && !on(&scene, &loaded, t.node, None) {
                scripted[o] = true;
                set(&mut active, &mut enabled, o, false);
                woke = true;
            }
        }
    }
    for e in scene.wreck_entries.iter().filter(|e| on(&scene, &active, e.node as i64, None)) {
        let wreck_seed = random.next_int(1, i32::MAX);
        super::load_instance(game, seed, e.position, e.rotation, difficulty, wreck_seed);
    }
    // ProceduralInstancesHandler.GenerateRandomPropsFromRoot: by the owner's first generator.
    if game.ext.planets.instances.instances.get(&seed).map_or(true, |l| l.is_empty()) {
        return;
    }
    let mut ignored = HashSet::new();
    let mut made = Vec::new();
    for (i, (node, position, rotation, props, ignore)) in scene.props.iter().enumerate() {
        if !on(&scene, &active, *node as i64, Some(scene.root)) || ignored.remove(&i) {
            continue;
        }
        let num = random.next_int(0, 101);
        let mut sum = 0;
        for (chance, group) in props {
            if num >= sum && num <= sum + chance {
                ignored.extend(ignore.iter().copied());
                // A prop without a group throws in the game, which ends the generation.
                let Some(group) = group else { return super::add_generated(game, seed, made) };
                made.extend(create(game, group, *position, *rotation));
                break;
            }
            sum += chance;
        }
    }
    super::add_generated(game, seed, made);
}

#[cfg(test)]
mod tests {
    use super::Shape;

    #[test]
    fn only_an_active_box_is_a_shape() {
        let row = |kind: &str, active: bool| serde_json::json!({"type": kind, "active": active, "position": [0, 0, 0], "rotation": [0, 0, 0, 1], "scale": [1, 1, 1], "center": [0, 0, 0], "size": [2, 2, 2]});
        let shape: Shape = serde_json::from_value(row("BoxCollider", true)).unwrap();
        assert!(shape.contains([0.5, 0.5, 0.5]) && !shape.contains([1.5, 0.0, 0.0]));
        // The state handoff writes it back the same way.
        let _: Shape = serde_json::from_value(serde_json::to_value(&shape).unwrap()).unwrap();
        assert!(serde_json::from_value::<Shape>(row("SphereCollider", true)).is_err());
        assert!(serde_json::from_value::<Shape>(row("BoxCollider", false)).is_err());
        let mut no_size = row("BoxCollider", true);
        no_size.as_object_mut().unwrap().remove("size");
        assert!(serde_json::from_value::<Shape>(no_size).is_err());
    }
}
