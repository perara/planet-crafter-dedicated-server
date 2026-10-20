//! What the host generates in procedural wrecks and world instances: GenerateOnTrigger.Generate
//! (the network objects of a wreck: safes, containers, locked doors, generators) and
//! WorldInstanceScene.InitSceneVariables (a world instance's variation, its wreck generators and
//! its shared random props), from wrecks.json and scenes/<planet>.json (native/).
//!
//! A wreck's layout is the game's own: Tessera (crate::tessera, a port of the game's solver) lays
//! it out from the wreck's seed, with the generator's tile set from wrecks.json. That takes
//! from tens of milliseconds to a few seconds, so it runs on a thread of its own between
//! `prepare` and `finish` (instances.rs queues the wrecks, one at a time as the game does).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use serde::Deserialize;

use crate::game::Game;
use crate::geometry::{from_euler_y, matrix_mul, matrix_position, matrix_rotation, trs, MathRandom, Matrix};
use crate::tessera;

/// RandomizePropData.RandomData: chance (percent), randomizeRotationY, the prop's group, the
/// key of the prop's own RandomizePropData list, and the prop's scene tiles (an index into
/// Wrecks::prefabs) when Tessera sees them.
type RandomData = (i32, bool, Option<String>, Option<String>, Option<usize>);
/// A RandomizePropData: the key of its list and its index there.
type PropId = (String, usize);

#[derive(Deserialize, Default)]
pub struct Wrecks {
    generators: Vec<GeneratorData>,
    props: HashMap<String, Vec<Slot>>,
    /// The scene tiles of the props the generators' own children can place.
    prefabs: Vec<tessera::TileNode>,
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
    /// The TesseraGenerator: tile set, constraints, palette.
    tessera: tessera::Generator,
    /// The scene tiles of the prefab itself, in the generator's space.
    fixed: Vec<tessera::SceneTile>,
    /// The solver's tables for it, built by the first wreck that needs them.
    #[serde(skip)]
    model: OnceLock<Result<Arc<tessera::Model>, String>>,
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
    /// wrecks.json: the generator of every wreck size, its tile set and the props lists.
    pub fn parse(text: &str) -> Result<Wrecks, String> {
        let w: Wrecks = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if w.generators.len() != super::GENERATORS.len() {
            return Err(format!("{} generators, where the game has {}", w.generators.len(), super::GENERATORS.len()));
        }
        Ok(w)
    }

    /// The solver's tables for a generator (TileModelInfo and the AdjacentModel built from it):
    /// about 10 ms, made once.
    fn model(&self, index: usize) -> Result<Arc<tessera::Model>, String> {
        let data = self.generators.get(index).ok_or(format!("no wreck generator {index}"))?;
        data.model.get_or_init(|| tessera::Model::new(data.tessera.clone()).map(Arc::new)).clone()
    }
}

/// wrecks.json, loaded at start.
pub fn wrecks(game: &Game) -> Arc<Wrecks> {
    game.scenes.wrecks.clone()
}

/// A world instance scene of the current planet; None for a name the planet has none of.
pub fn instance_scene(game: &Game, name: &str) -> Option<Arc<InstanceScene>> {
    game.scenes.planets.get(game.planet_id())?.instance_scenes.get(name).cloned()
}

/// GenerateOnTrigger's trigger sphere radius, the scene wrecks' (owner 0) scaled down
/// (OnNetworkSpawn's _sceneInstanceRadiusRatio).
pub fn trigger_radius(game: &Game, index: usize, owner: i32) -> f32 {
    let g = &game.scenes.wrecks.generators[index];
    let radius = g.triggers.iter().map(|t| t.radius).fold(0.0, f32::max);
    if owner == 0 && g.scene_instance_radius_ratio > 0.0 { radius * g.scene_instance_radius_ratio } else { radius }
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

/// One pass of GenerateOnTrigger.InstantiateRandomPropsInHierarchy over a decoration: the
/// random numbers, the RandomizePropData flags, and what it picks.
struct Walk<'a> {
    w: &'a Wrecks,
    random: MathRandom,
    ignored: HashSet<PropId>,
    /// The network slots it buffers, with their world matrices.
    buffer: Vec<(PropId, Matrix)>,
    /// The props it picks (their scene tiles), with their matrices in the prefab's space.
    made: Vec<(Option<usize>, Matrix)>,
}

impl Walk<'_> {
    /// The server's half of the walk: the props picked for the decoration (whose own props are
    /// walked next) and the network slots it buffers. `parent` places them in the world,
    /// `local` (a pass that wants `made`) in the prefab's own space.
    fn walk(&mut self, key: &str, ltw: &Matrix, parent: &Matrix, local: Option<&Matrix>) {
        let mut picked = Vec::new();
        for (i, slot) in self.w.props.get(key).into_iter().flatten().enumerate() {
            let id = (key.to_string(), i);
            if slot.network {
                if !self.ignored.remove(&id) {
                    self.buffer.push((id, matrix_mul(&matrix_mul(parent, ltw), &affine(&slot.m))));
                }
                continue;
            }
            if let Some(d) = retrieve(&id, &slot.props, &slot.ignore, &mut self.random, &mut self.ignored) {
                let mut m = affine(&slot.m);
                if d.1 {
                    m = matrix_mul(&m, &trs([0.0; 3], from_euler_y(self.random.next_float() * 360.0), [1.0; 3]));
                }
                if let Some(local) = local {
                    self.made.push((d.4, matrix_mul(&matrix_mul(local, ltw), &m)));
                }
                picked.push((d.3.clone(), m));
            }
        }
        for (prop, m) in picked {
            if let Some(prop) = prop {
                self.walk(&prop, &matrix_mul(ltw, &m), parent, local);
            }
        }
    }
}

/// Matrix4x4.lossyScale for the matrices these are (rotation and scale): column lengths.
fn lossy(m: &Matrix) -> [f32; 3] {
    std::array::from_fn(|j| (m[0][j] * m[0][j] + m[1][j] * m[1][j] + m[2][j] * m[2][j]).sqrt())
}

/// What GenerateOnTrigger.Generate does before Tessera: the walk of the generator's other
/// children from the seed. Its scene tiles are what Tessera takes as constraints.
pub struct Prepared {
    pub scene: Vec<tessera::SceneTile>,
    buffer: Vec<(PropId, Matrix)>,
    pub ignored: HashSet<PropId>,
}

/// The tile positions Tessera chose, as the walk takes them: (props key, local position, local rotation).
pub type Layout = Vec<(String, [f32; 3], [f32; 4])>;

/// How many seeds one wreck may try (the game goes on until Tessera succeeds; its failures are
/// one in thirty or so).
const MAX_TRIES: u32 = 1000;

/// The part of Generate before Tessera, `ignored` being the RandomizePropData flags the
/// generations before left set (the game keeps them on the prefabs). None for a generator the
/// tables do not have.
pub fn prepare(game: &Game, index: usize, seed: u32, position: [f32; 3], rotation: [f32; 4], mut ignored: HashSet<PropId>) -> Option<Prepared> {
    let w = wrecks(game);
    let data = w.generators.get(index)?;
    // The generator's own children are its instance's (fresh flags); tiles and props are prefabs.
    ignored.retain(|(key, _)| data.children.iter().all(|c| c.props != *key));
    // GenerateRandomPropsFromRoot on the generator's other children (its container stays empty
    // on a server): the network slots in the world, the props' places in the prefab's space.
    let local = affine(&data.container);
    let container = matrix_mul(&trs(position, rotation, data.scale), &local);
    let mut walk = Walk { w: &w, random: MathRandom::new(seed), ignored, buffer: Vec::new(), made: Vec::new() };
    for child in &data.children {
        walk.walk(&child.props, &affine(&child.ltw), &container, Some(&local));
    }
    // Tessera sees the tiles of the prefab and of the props placed.
    let inverse = tessera::inverse(&affine(&data.generator));
    let mut scene = data.fixed.clone();
    for (prefab, m) in walk.made {
        let Some(tree) = prefab.and_then(|i| w.prefabs.get(i)) else { continue };
        let l = lossy(&m);
        let at = trs(matrix_position(&m), matrix_rotation(&m), std::array::from_fn(|i| tree.s[i] * l[i]));
        tree.scene_tiles(&at, &inverse, &mut scene);
    }
    Some(Prepared { scene, buffer: walk.buffer, ignored: walk.ignored })
}

/// Tessera from the seed, the next seed while it fails: the wreck's tiles. Runs on a thread.
pub fn solve(w: &Wrecks, index: usize, seed: u32, scene: &[tessera::SceneTile]) -> Result<Layout, String> {
    let model = w.model(index)?;
    for current in seed..seed.saturating_add(MAX_TRIES) {
        if let Some(tiles) = tessera::generate(&model, scene, current as i32)? {
            return Ok(tiles.iter().filter_map(|t| Some((model.tiles[t.tile].props.clone()?, t.position, t.rotation))).collect());
        }
    }
    Err(format!("Tessera failed on {MAX_TRIES} seeds from {seed}"))
}

/// The private CreateAndInstantiateWorldObject: a generated object, with what its inventory
/// was filled with (GetGeneratedNetworkObjectIds).
fn create(game: &mut Game, group: &str, position: [f32; 3], rotation: [f32; 4]) -> Vec<i32> {
    let Some(group) = game.data.group(group) else { return Vec::new() };
    let (id, _) = game.create_and_instantiate(group, position, rotation, false, true, true);
    crate::machines::growers::announce_growth(game, id);
    let inventory = game.wos[&id].linked_inventory;
    let mut ids = vec![id];
    ids.extend(game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default());
    ids
}

/// The rest of GenerateOnTrigger.Generate on the server, once Tessera has laid the wreck out:
/// the tiles' props, then the network objects of a generator's wreck (safes, containers,
/// locked doors, generators). Returns the generated world objects and the flags left set.
pub fn finish(game: &mut Game, index: usize, seed: u32, position: [f32; 3], rotation: [f32; 4], prepared: Prepared, layout: &Layout) -> (Vec<i32>, HashSet<PropId>) {
    let w = wrecks(game);
    let Some(data) = w.generators.get(index) else { return (Vec::new(), prepared.ignored) };
    let root = trs(position, rotation, data.scale);
    let parent = matrix_mul(&root, &affine(&data.generator));
    let mut walk = Walk { w: &w, random: MathRandom::new(seed), ignored: prepared.ignored, buffer: prepared.buffer, made: Vec::new() };
    for (tile, p, q) in layout {
        walk.walk(tile, &trs(*p, *q, [1.0; 3]), &parent, None);
    }
    let Walk { buffer, mut ignored, .. } = walk;
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
        let Some((_, rotate, Some(group), ..)) = retrieve(&id, &slot.props, &slot.ignore, &mut random, &mut ignored) else { continue };
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
    (made, ignored)
}

/// A wreck laid out now, on this thread (what instances.rs does over several ticks).
#[cfg(test)]
pub fn generate_now(game: &mut Game, index: usize, seed: u32, position: [f32; 3], rotation: [f32; 4]) -> Vec<i32> {
    let ignored = std::mem::take(&mut game.ext.planets.instances.ignored);
    let prepared = prepare(game, index, seed, position, rotation, ignored).unwrap();
    let layout = solve(&wrecks(game), index, seed, &prepared.scene).unwrap();
    let (made, ignored) = finish(game, index, seed, position, rotation, prepared, &layout);
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
    if game.ext.planets.instances.instances.get(&seed).is_none_or(|l| l.is_empty()) {
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
    use super::*;

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

    /// Tessera is deterministic in the seed, and a seed whose retries all contradict goes on to
    /// the next, as GenerateOnTrigger.Generate does (read-game measured seed 14 of the small
    /// wreck: every retry fails).
    #[test]
    fn the_same_seed_gives_the_same_layout_and_a_failing_seed_moves_on() {
        let Some(g) = crate::planets::tests::game() else { return };
        let w = wrecks(&g);
        let prepare = |seed| prepare(&g, 0, seed, [0.0; 3], [0.0, 0.0, 0.0, 1.0], HashSet::new()).unwrap();
        let model = w.model(0).unwrap();
        let scene = prepare(14).scene;
        assert!(tessera::generate(&model, &scene, 14).unwrap().is_none(), "seed 14 contradicts");
        let moved = solve(&w, 0, 14, &scene).unwrap();
        let next: Layout = tessera::generate(&model, &scene, 15).unwrap().expect("seed 15 lays out").iter().filter_map(|t| Some((model.tiles[t.tile].props.clone()?, t.position, t.rotation))).collect();
        assert_eq!(moved, next);
        assert_eq!(solve(&w, 0, 14, &scene).unwrap(), moved);
        // Another seed, another wreck.
        let other = prepare(16);
        assert_ne!(solve(&w, 0, 16, &other.scene).unwrap(), moved);
    }
}
