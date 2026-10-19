//! Growth: GrowthProxy, MachineGrowerIfLinkedGroup with MachineConvertRecipe (genetic
//! manipulator, incubator), MachineGrowerVegetationStatic (vegetubes, spreaders, planters) and
//! MachineGrowerVegetationHarvestable (vegetable growers, farms, algae).

use serde_json::{json, Value};

use super::{f32_of, group_of_game_object, group_ref, has_energy, is_active, is_full, items, net_has, remove_item, Game};
use crate::geometry::MathRandom;
use crate::net::{enc, Target};

/// Mathf.Epsilon-sized comparisons the growers use (float.Epsilon of Unity.Mathematics).
const EPSILON: f32 = 1.192_092_9e-7;

/// The float draws of Unity.Mathematics.Random the growers use. Static growers send the
/// random state to clients, which replay the same sequence, so it has to match bit for bit.
pub trait Floats {
    fn range(&mut self, min: f32, max: f32) -> f32;
    fn direction(&mut self) -> [f32; 2];
}

impl Floats for MathRandom {
    fn range(&mut self, min: f32, max: f32) -> f32 {
        self.next_float() * (max - min) + min
    }
    /// NextFloat2Direction.
    fn direction(&mut self) -> [f32; 2] {
        let angle = self.next_float() * std::f32::consts::PI * 2.0;
        [angle.cos(), angle.sin()]
    }
}

// ---- quaternions -------------------------------------------------------------------------

pub fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [aw * bx + ax * bw + ay * bz - az * by, aw * by - ax * bz + ay * bw + az * bx, aw * bz + ax * by - ay * bx + az * bw, aw * bw - ax * bx - ay * by - az * bz]
}

/// q * v.
pub fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let p = mul(mul(q, [v[0], v[1], v[2], 0.0]), [-q[0], -q[1], -q[2], q[3]]);
    [p[0], p[1], p[2]]
}

fn axis_angle(axis: usize, degrees: f32) -> [f32; 4] {
    let h = degrees.to_radians() / 2.0;
    let mut q = [0.0, 0.0, 0.0, h.cos()];
    q[axis] = h.sin();
    q
}

/// Quaternion.LookRotation(forward) with the world up; forward along up falls back to a turn
/// about X, as Unity does.
pub fn look_rotation(f: [f32; 3]) -> [f32; 4] {
    let len = (f[0] * f[0] + f[1] * f[1] + f[2] * f[2]).sqrt();
    let z = f.map(|v| v / len);
    let mut x = [z[2], 0.0, -z[0]]; // cross(up, z)
    let xl = (x[0] * x[0] + x[2] * x[2]).sqrt();
    if xl < 1e-6 {
        x = [1.0, 0.0, 0.0];
    } else {
        x = x.map(|v| v / xl);
    }
    let y = [z[1] * x[2] - z[2] * x[1], z[2] * x[0] - z[0] * x[2], z[0] * x[1] - z[1] * x[0]];
    // Rotation matrix (columns x, y, z) to quaternion.
    let (m00, m11, m22) = (x[0], y[1], z[2]);
    let trace = m00 + m11 + m22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [(y[2] - z[1]) / s, (z[0] - x[2]) / s, (x[1] - y[0]) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [0.25 * s, (y[0] + x[1]) / s, (z[0] + x[2]) / s, (y[2] - z[1]) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [(y[0] + x[1]) / s, 0.25 * s, (z[1] + y[2]) / s, (z[0] - x[2]) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [(z[0] + x[2]) / s, (z[1] + y[2]) / s, 0.25 * s, (x[1] - y[0]) / s]
    };
    if q[3] < 0.0 { q.map(|v| -v) } else { q }
}

// ---- GrowthProxy -------------------------------------------------------------------------

/// GrowthProxy.GetGrowth: the world object's growth (the proxy only caches it).
pub fn growth(game: &Game, wo: i32) -> f32 {
    game.wos.get(&wo).map(|w| w.growth).unwrap_or(0.0)
}

/// GrowthProxy.SetGrowth on the server: the world object keeps it; 0 (or forced) goes out.
pub fn set_growth(game: &mut Game, wo: i32, value: f32, force: bool) {
    if let Some(w) = game.wos.get_mut(&wo) {
        w.growth = value;
    }
    if (value == 0.0 || force) && game.wos.get(&wo).is_some_and(|w| w.net.is_some()) {
        reset_current_growth(game, wo, value);
    }
}

/// GrowthProxy.ResetCurrentGrowthServerRpc.
pub fn reset_current_growth(game: &mut Game, wo: i32, value: f32) {
    let Some(w) = game.wos.get_mut(&wo) else { return };
    w.growth = value;
    if let Some(net) = w.net {
        game.net.rpc(Target::All, net, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(value)]);
    }
}

/// The earliest time tick has anything to do (the times tick checks, in one place).
pub(super) fn next_due(m: &super::Machine) -> f32 {
    let vegetation = m.vegetation.as_ref().and_then(|v| match v.coroutine {
        Coroutine::Waiting(t) | Coroutine::Growing(t) => Some(t),
        Coroutine::None => None,
    });
    [m.linked_grower.as_ref().map(|g| g.next), m.converter, vegetation, m.harvestable.as_ref().and_then(|h| h.next)].into_iter().flatten().fold(f32::INFINITY, f32::min)
}

pub fn tick(game: &mut Game, wo: i32, now: f32) {
    let Some(m) = game.ext.machines.list.get(&wo) else { return };
    let linked_due = m.linked_grower.as_ref().is_some_and(|g| now >= g.next);
    let convert_due = m.converter.is_some_and(|t| now >= t);
    let vegetation = m.vegetation.as_ref().map(|v| v.coroutine);
    let harvest_due = m.harvestable.as_ref().and_then(|h| h.next).is_some_and(|t| now >= t);
    if linked_due {
        linked_grower_update(game, wo, now);
    }
    if convert_due {
        game.ext.machines.get_mut(&wo).unwrap().converter = Some(now + 2.0);
        convert_if_fully_grown(game, wo);
    }
    match vegetation {
        Some(Coroutine::Waiting(t)) if now >= t => launch_growing(game, wo),
        Some(Coroutine::Growing(t)) if now >= t => {
            let v = game.ext.machines.get_mut(&wo).unwrap().vegetation.as_mut().unwrap();
            v.coroutine = Coroutine::Growing(now + v.interval);
            grow(game, wo);
        }
        _ => {}
    }
    if harvest_due {
        let h = game.ext.machines.get_mut(&wo).and_then(|m| m.harvestable.as_mut()).unwrap();
        h.next = Some(now + h.interval);
        update_content_growth(game, wo);
    }
}

// ---- MachineGrowerIfLinkedGroup and MachineConvertRecipe ----------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
pub struct LinkedGrower {
    next: f32,
    rate: f32,
}

const LINKED_UPDATE_EVERY: f32 = 2.0;

impl LinkedGrower {
    /// Awake: the update loop starts once the local player has (on the server: at once).
    pub fn new(f: &Value, now: f32) -> LinkedGrower {
        let time_to_grow = f32_of(f, "timeToGrow");
        LinkedGrower { next: now, rate: 100.0 / (time_to_grow * 60.0 / LINKED_UPDATE_EVERY) }
    }
}

fn linked_groups(game: &Game, wo: i32) -> Vec<usize> {
    game.wos.get(&wo).and_then(|w| w.linked_groups.clone()).unwrap_or_default()
}

/// MachineGrowerIfLinkedGroup.UpdateGrowth, every 2 s.
fn linked_grower_update(game: &mut Game, wo: i32, now: f32) {
    let g = game.ext.machines.get_mut(&wo).unwrap().linked_grower.as_mut().unwrap();
    g.next = now + LINKED_UPDATE_EVERY;
    let rate = g.rate;
    if !has_energy(game, wo) || !net_has(game, wo, "GrowthProxy") || !net_has(game, wo, "LinkedGroupsProxy") {
        return;
    }
    let current = growth(game, wo);
    if current < 100.0 {
        if !linked_groups(game, wo).is_empty() {
            set_growth(game, wo, current + rate, false);
        }
    } else {
        set_growth(game, wo, 100.0, false);
    }
}

/// MachineConvertRecipe.CheckIfFullyGrown: a fully grown recipe replaces the inventory's
/// content with the linked group.
fn convert_if_fully_grown(game: &mut Game, wo: i32) {
    let inventory = game.ext.machines.list[&wo].linked;
    if !net_has(game, wo, "LinkedGroupsProxy") || !net_has(game, wo, "GrowthProxy") || inventory == 0 {
        return;
    }
    let linked = linked_groups(game, wo);
    if linked.is_empty() || (growth(game, wo).min(100.0) - 100.0).abs() >= EPSILON {
        return;
    }
    super::remove_and_destroy_all(game, inventory);
    super::add_new(game, linked[0], inventory);
    super::set_linked_groups(game, wo, &[]);
    set_growth(game, wo, 0.0, false);
}

// ---- MachineGrowerVegetationStatic -------------------------------------------------------

/// The static grower's running coroutine.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Coroutine {
    None,
    /// WaitForInitBeforeGrowing, until then.
    Waiting(f32),
    /// UpdateGrowing, next Grow then.
    Growing(f32),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Vegetation {
    random: MathRandom,
    original: Vec<String>,
    things: Vec<String>,
    grow_speed: f32,
    interval: f32,
    min_growth: f32,
    coroutine: Coroutine,
}

impl Vegetation {
    /// SetWorldObjectForGrower: the random sequence seeded by the world object id, the growth
    /// from the world object, then the first LaunchGrowingProcess (wait 2 s for init).
    pub fn new(f: &Value, wo: i32, now: f32) -> Vegetation {
        let things: Vec<String> = f["thingsToGrow"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        Vegetation {
            random: MathRandom::new(wo as u32),
            original: things.clone(),
            things,
            grow_speed: f32_of(f, "growSpeed"),
            interval: f32_of(f, "growthUpdateInterval"),
            min_growth: f32_of(f, "minGrowth"),
            coroutine: Coroutine::Waiting(now + 2.0),
        }
    }
}

/// UpdateThingsToGrowFromInventory: the prefab's things plus what the seeds inside grow.
fn update_things(game: &mut Game, wo: i32) {
    let inventory = game.ext.machines.list[&wo].linked;
    let mut things = game.ext.machines.list[&wo].vegetation.as_ref().unwrap().original.clone();
    for item in items(game, inventory) {
        let Some(g) = game.wos.get(&item).map(|w| w.group) else { continue };
        if let Some(go) = group_ref(game, g, "growableGroup").and_then(|grown| game.data.groups[grown].fields["associatedGameObject"].as_str()) {
            if !things.iter().any(|t| t == go) {
                things.push(go.to_string());
            }
        }
    }
    game.ext.machines.get_mut(&wo).unwrap().vegetation.as_mut().unwrap().things = things;
}

/// SetGrowerInventory.
pub fn vegetation_inventory_set(game: &mut Game, wo: i32) {
    let inventory = game.ext.machines.list[&wo].linked;
    if !game.ext.machines.list[&wo].vegetation.as_ref().unwrap().things.is_empty() || !items(game, inventory).is_empty() {
        update_things(game, wo);
    }
}

/// LaunchGrowingProcess after the first: the growing loop restarts if there is something to grow.
fn launch_growing(game: &mut Game, wo: i32) {
    let now = game.time;
    let v = game.ext.machines.get_mut(&wo).unwrap().vegetation.as_mut().unwrap();
    if v.interval <= 0.1 {
        v.interval = 0.1;
    }
    v.coroutine = if v.things.is_empty() { Coroutine::None } else { Coroutine::Growing(now + v.interval) };
}

/// Grow.
fn grow(game: &mut Game, wo: i32) {
    if !has_energy(game, wo) {
        return;
    }
    let multiplier = game.ext.machines.list[&wo].grow_multiplier;
    let v = game.ext.machines.get_mut(&wo).unwrap().vegetation.as_mut().unwrap();
    if v.things.is_empty() {
        return;
    }
    let num = v.grow_speed * multiplier * v.interval * v.random.next_float();
    let value = (growth(game, wo) + num).min(100.0);
    set_growth(game, wo, value, false);
}

/// OnInventoryModified: new seeds restart growth (from minGrowth, or 0 when one went), and
/// every client replays from the new random state.
pub fn vegetation_inventory_modified(game: &mut Game, wo: i32, added: bool) {
    let root_proxy_is_primary = super::components(game, game.wos[&wo].group, "InventoryAssociatedProxy", true).first().is_some_and(|p| p["_secondaryInventoryIndex"].as_i64().unwrap_or(-1) < 0);
    if root_proxy_is_primary {
        update_things(game, wo);
    }
    let min_growth = game.ext.machines.list[&wo].vegetation.as_ref().unwrap().min_growth;
    set_growth(game, wo, if added { min_growth * 100.0 } else { 0.0 }, false);
    launch_growing(game, wo);
    let state = game.ext.machines.list[&wo].vegetation.as_ref().unwrap().random.state;
    let value = growth(game, wo);
    if let Some(net) = game.wos.get(&wo).and_then(|w| w.net) {
        game.net.rpc(Target::All, net, "ConstructibleProxy.MachineOutsideGrowerUpdateStartClientRpc", &[json!(state), json!(value)]);
    }
}

/// MachineOutsideGrowerUpdateStartServerRpc: (random state, world object growth).
pub fn outside_grower_state(game: &Game, wo: i32) -> Option<(u32, f32)> {
    let v = game.ext.machines.list.get(&wo)?.vegetation.as_ref()?;
    Some((v.random.state, game.wos.get(&wo)?.growth))
}

// ---- MachineGrowerVegetationHarvestable --------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Harvestable {
    /// The second inventory (secondInventoryAssociatedProxy: the first secondary inventory).
    pub second: Option<i32>,
    /// Items still growing, with the network object they had.
    growings: Vec<(i32, u64)>,
    /// UpdateContentGrowth, next run.
    next: Option<f32>,
    interval: f32,
    grow_speed: f32,
    min_growth: f32,
    min_radius: f32,
    radius: f32,
    spawn_on_this: bool,
    down_value: f32,
    /// allowedLayers is the Water layer alone (1 << 4): a spot is where a ray down meets water.
    /// From the tables, so not in a snapshot (a snapshot of a release before it has none):
    /// `refresh` sets it again after a restore.
    #[serde(skip)]
    water_only: bool,
    things: Vec<String>,
    model_to_override: Option<String>,
}

impl Harvestable {
    fn water_only_of(f: &Value) -> bool {
        f["allowedLayers"].as_i64() == Some(1 << 4)
    }

    /// What comes from the tables and is not in a snapshot, set again after a restore.
    pub fn refresh(&mut self, f: &Value) {
        self.water_only = Self::water_only_of(f);
    }

    pub fn new(f: &Value) -> Harvestable {
        Harvestable {
            second: None,
            growings: Vec::new(),
            next: None,
            interval: f32_of(f, "growthUpdateInterval"),
            grow_speed: f32_of(f, "growSpeed"),
            min_growth: f32_of(f, "minGrowth"),
            min_radius: f32_of(f, "minRadius"),
            radius: f32_of(f, "radius"),
            spawn_on_this: f["spawnOnThis"].is_string(),
            down_value: f32_of(f, "downValue"),
            water_only: Self::water_only_of(f),
            things: f["thingsToGrow"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
            model_to_override: f["modelToOverride"].as_str().map(str::to_string),
        }
    }
}

fn harvestable(game: &mut Game, wo: i32) -> Option<&mut Harvestable> {
    game.ext.machines.get_mut(&wo)?.harvestable.as_mut()
}

/// Init (a frame after Awake, once the local player started): watch the second inventory,
/// fill it, and start the growth loop.
pub fn harvestable_init(game: &mut Game, wo: i32) {
    let Some(second) = game.wos.get(&wo).and_then(|w| w.secondary_inventories.first().copied()) else { return };
    if !game.inventories.contains_key(&second) {
        return;
    }
    harvestable(game, wo).unwrap().second = Some(second);
    game.ext.machines.listening.insert(second, wo);
    for item in items(game, second) {
        second_inventory_modified(game, wo, item, true);
    }
    seed_inventory_modified(game, wo, false, true);
    let now = game.time;
    harvestable(game, wo).unwrap().next = Some(now);
}

/// OnInventoryModified of the seed inventory: what is still growing goes; then the second
/// inventory fills up again.
pub fn seed_inventory_modified(game: &mut Game, wo: i32, modified: bool, added: bool) {
    let Some(second) = harvestable(game, wo).and_then(|h| h.second) else { return };
    if modified {
        for item in items(game, second).into_iter().rev() {
            if game.wos.get(&item).is_some_and(|w| w.growth < 100.0) {
                game.destroy_world_object(item, true);
            }
        }
    }
    if !added {
        return;
    }
    let mut n = 0;
    while !is_full(game, second) {
        create_new_growable(game, wo);
        if n == 100 {
            break;
        }
        n += 1;
    }
}

/// OnSecondInventoryModified: an item that came in grows (and is harvested) where it is.
pub fn second_inventory_modified(game: &mut Game, wo: i32, item: i32, added: bool) {
    if !added {
        return;
    }
    let Some(net) = game.wos.get(&item).and_then(|w| w.net) else { return };
    let value = growth(game, item);
    // SetupObjectOverride: the item shows as another group.
    if let Some(model) = harvestable(game, wo).and_then(|h| h.model_to_override.clone()) {
        if let Some(g) = group_of_game_object(game, &model) {
            let hash = game.data.groups[g].hash;
            game.net.set(net, "GroupNetworkContainer", "_groupHashOverride", enc::int(hash));
        }
    }
    game.ext.machines.grab.insert(item, (wo, net));
    if value < 100.0 {
        harvestable(game, wo).unwrap().growings.push((item, net));
    }
}

/// CreateNewGrowable: a new item of what the seed grows, on the growing spot, in the second
/// inventory.
fn create_new_growable(game: &mut Game, wo: i32) -> bool {
    let Some(h) = game.ext.machines.list.get(&wo).and_then(|m| m.harvestable.as_ref()) else { return false };
    let (second, things) = (h.second, h.things.clone());
    let linked = game.ext.machines.list[&wo].linked;
    let seeds = items(game, linked);
    let group = if linked != 0 && !seeds.is_empty() {
        game.wos.get(&seeds[0]).and_then(|w| group_ref(game, w.group, "growableGroup"))
    } else if !things.is_empty() {
        let pick = game.rng.range(0, things.len() as i32) as usize;
        group_of_game_object(game, &things[pick])
    } else {
        None
    };
    let (Some(group), Some(second)) = (group, second) else { return false };
    let Some(machine) = game.wos.get(&wo) else { return false };
    let (mut position, machine_rotation, planet) = (machine.position, machine.rotation, machine.planet_hash());
    let mut rotation = [0.0, 0.0, 0.0, 1.0];
    if is_active(game, wo) {
        let mut random = MathRandom::new(game.rng.range(1, 9999) as u32);
        // CreateNewGrowable gives up after 52 tries (no spot, no item).
        let Some((p, normal)) = random_position(game, wo, &mut random, machine_rotation, 52) else { return false };
        position = p;
        rotation = mul(look_rotation(normal), mul(axis_angle(2, random.range(0.0, 360.0)), axis_angle(0, 90.0)));
    }
    // The spawn callback's SetActive(machine active) and SetPlanetHash(the machine's): the item
    // belongs to the machine's planet from its spawn on, _active included.
    let item = create_and_instantiate(game, group, position, rotation, planet);
    let min_growth = harvestable(game, wo).map(|h| h.min_growth).unwrap_or(0.0);
    set_growth(game, item, (min_growth * 100.0).max(EPSILON), false);
    game.add_existing_item(item, second, false, 0);
    true
}

/// OnEnable on the server, once the second inventory is known: the session arrived on the
/// grower's planet. Items grown back while it was away were spawned on the grower's pivot
/// (CreateNewGrowable on an inactive grower); each moves onto a growing spot.
pub fn harvestable_enabled(game: &mut Game, wo: i32) {
    let Some(second) = harvestable(game, wo).and_then(|h| h.second) else { return };
    let (pivot, machine_rotation) = (game.wos[&wo].position, game.wos[&wo].rotation);
    let mut random = MathRandom::new(game.rng.range(1, 9999) as u32);
    for item in items(game, second) {
        let Some(at) = game.wos.get(&item).map(|w| w.position) else { continue };
        if (0..3).map(|i| (at[i] - pivot[i]) * (at[i] - pivot[i])).sum::<f32>() >= EPSILON {
            continue;
        }
        // OnEnable gives up after 22 tries and leaves the item on the pivot.
        let Some((position, normal)) = random_position(game, wo, &mut random, machine_rotation, 22) else { continue };
        let rotation = mul(look_rotation(normal), mul(axis_angle(2, random.range(0.0, 360.0)), axis_angle(0, 90.0)));
        let w = game.wos.get_mut(&item).unwrap();
        (w.position, w.rotation) = (position, rotation);
        if let Some(net) = w.net {
            game.net.move_object(net, position, Some(rotation));
        }
    }
}

/// The growing spots (spawnOnThis) of the prefabs that grow on one, in the machine's frame, and
/// the height of the soil others raycast onto; measured on the game (the dump has no transforms).
const SPOTS: &[(&str, [f32; 3], [f32; 3])] = &[("VegetableGrower1", [0.00476, 1.249, 0.03], [-0.005, 0.992, -0.1265]), ("VegetableGrower2", [0.00476, 1.249, 0.03], [-0.005, 0.992, -0.1265])];
const SOILS: &[(&str, f32)] = &[("Farm1", 0.618_337), ("Farm2", 0.826_295)];

/// GetRandomPosition: the spot an item grows on, in at most `tries` tries (None: none found).
// ponytail: the game raycasts down onto the soil or the water within the ring; the server has no
// soil colliders, so the ring is taken as level at the measured soil height and every try hits.
// Water is real (water::surface_below), on a planet whose water volumes are known: the algae
// generators grow nothing where there is no water. Upgrade: dump the spots and soil colliders.
fn random_position(game: &mut Game, wo: i32, random: &mut MathRandom, machine_rotation: [f32; 4], tries: u32) -> Option<([f32; 3], [f32; 3])> {
    let h = game.ext.machines.list[&wo].harvestable.as_ref().unwrap();
    let (radius, min_radius, down, spawn_on_this, water_only) = (h.radius, h.min_radius, h.down_value, h.spawn_on_this, h.water_only);
    let base = game.wos[&wo].position;
    let id = game.data.groups[game.wos[&wo].group].id.as_str();
    if radius == 0.0 && spawn_on_this {
        // The soil's normal leans a little (measured from the crops' turns on the game).
        let (spot, normal) = SPOTS.iter().find(|(g, _, _)| *g == id).map(|(_, s, n)| (*s, *n)).unwrap_or(([0.0; 3], [0.0, 1.0, 0.0]));
        let offset = rotate(machine_rotation, spot);
        return Some(([0, 1, 2].map(|i| base[i] + offset[i]), rotate(machine_rotation, normal)));
    }
    let soil = SOILS.iter().find(|(g, _)| *g == id).map(|(_, s)| *s).unwrap_or(0.0);
    let water = water_only && crate::water::known(game);
    for _ in 0..tries {
        let [dx, dz] = random.direction();
        let r = random.range(min_radius, radius);
        let (x, z) = (base[0] + dx * r, base[2] + dz * r);
        if !water {
            return Some(([x, base[1] + soil - down, z], [0.0, 1.0, 0.0]));
        }
        // The ray starts 5 m above the machine and reaches 50 m down.
        if let Some(surface) = crate::water::surface_below(game, [x, base[1] + 5.0, z], 50.0) {
            return Some(([x, surface - down, z], [0.0, 1.0, 0.0]));
        }
    }
    None
}

/// CreateAndInstantiateWorldObjectServerRpc as the server calls it (no position check, saved),
/// on `planet`.
fn create_and_instantiate(game: &mut Game, group: usize, position: [f32; 3], rotation: [f32; 4], planet: i32) -> i32 {
    let id = game.new_world_object_id();
    game.create_world_object(group, id, true);
    let w = game.wos.get_mut(&id).unwrap();
    w.position = position;
    w.rotation = rotation;
    w.planet = planet;
    let owner = if game.data.groups[group].usable_type() == "Buildable" { game.nearest_client(position) } else { 0 };
    game.instantiate(id, false, owner);
    id
}

/// OnVegetableGrabed: the harvested item leaves the second inventory and a new one starts.
pub fn vegetable_grabbed(game: &mut Game, wo: i32, item: i32) {
    let Some(second) = harvestable(game, wo).and_then(|h| h.second) else { return };
    remove_item(game, item, second, false);
    create_new_growable(game, wo);
}

/// UpdateContentGrowth: every item grows by the same step until it can be harvested.
fn update_content_growth(game: &mut Game, wo: i32) {
    if !has_energy(game, wo) {
        return;
    }
    let multiplier = game.ext.machines.list[&wo].grow_multiplier;
    let h = harvestable(game, wo).unwrap();
    let step = h.grow_speed * multiplier * h.interval;
    for i in (0..h.growings.len()).rev() {
        let Some(&(item, net)) = harvestable(game, wo).unwrap().growings.get(i) else { continue };
        if !game.wos.contains_key(&item) || !game.net.objects.contains_key(&net) {
            harvestable(game, wo).unwrap().growings.remove(i);
            continue;
        }
        let value = (growth(game, item) + step).min(100.0);
        let done = (value - 100.0).abs() < EPSILON;
        set_growth(game, item, value, false);
        if done {
            harvestable(game, wo).unwrap().growings.remove(i);
            // Inventory.inventoryContentGrowthFinished of the second inventory.
            if let Some(second) = harvestable(game, wo).and_then(|h| h.second) {
                crate::rockets::inventory_growth_finished(game, second, item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::tests::{build, game};

    #[test]
    fn math_random_is_unity_xorshift() {
        let mut r = MathRandom::new(1);
        assert_eq!(r.state, 270369);
        assert_eq!(r.next_float(), f32::from_bits(0x3f80_0000 | (270369 >> 9)) - 1.0);
        let mut a = MathRandom::new(203_372_963);
        let mut b = MathRandom { state: a.state };
        assert_eq!(a.next_float(), b.next_float());
        assert!((0..1000).map(|_| a.next_float()).all(|v| (0.0..1.0).contains(&v)));
        let d = MathRandom::new(7).direction();
        assert!((d[0] * d[0] + d[1] * d[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn look_rotation_matches_unity() {
        let up = look_rotation([0.0, 1.0, 0.0]);
        let s = std::f32::consts::FRAC_1_SQRT_2;
        assert!(up.iter().zip([-s, 0.0, 0.0, s]).all(|(a, b)| (a - b).abs() < 1e-5), "{up:?}");
        let fwd = look_rotation([0.0, 0.0, 1.0]);
        assert!(fwd.iter().zip([0.0, 0.0, 0.0, 1.0]).all(|(a, b)| (a - b).abs() < 1e-5));
        let right = rotate(look_rotation([1.0, 0.0, 0.0]), [0.0, 0.0, 1.0]);
        assert!((right[0] - 1.0).abs() < 1e-5);
        // Flat ground: the item turns about the vertical only.
        let q = mul(up, mul(axis_angle(2, 30.0), axis_angle(0, 90.0)));
        let y = rotate(q, [0.0, 1.0, 0.0]);
        assert!((y[1] - 1.0).abs() < 1e-5, "{y:?}");
    }

    #[test]
    fn vegetubes_grow_with_energy_and_reset_on_new_seeds() {
        let Some(mut game) = game() else { return };
        let tube = build(&mut game, "Vegetube1", [10.0, 5.0, 0.0]);
        let inv = game.wos[&tube].linked_inventory;
        assert_eq!(outside_grower_state(&game, tube), Some((MathRandom::new(tube as u32).state, 0.0)));
        game.add_new_item(game.data.group("Seed0").unwrap(), inv, 0);
        assert_eq!(game.wos[&tube].growth, 0.3f32 * 100.0, "a new seed starts from minGrowth");
        game.ext.machines.energy.insert(tube, true);
        let (state, start) = outside_grower_state(&game, tube).unwrap();
        for step in 1..=3 {
            game.time += 1.0;
            crate::machines::tick(&mut game);
            let _ = step;
        }
        let mut replay = MathRandom { state };
        let mut expected = start;
        for _ in 0..3 {
            expected = (expected + 0.05 * 1.0 * 1.0 * replay.next_float()).min(100.0);
        }
        assert_eq!(game.wos[&tube].growth, expected);
        game.ext.machines.energy.insert(tube, false);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&tube].growth, expected, "no energy, no growth");
    }

    #[test]
    fn incubators_grow_when_linked_and_convert_when_grown() {
        let Some(mut game) = game() else { return };
        let incubator = build(&mut game, "Incubator1", [0.0, 5.0, 0.0]);
        game.ext.machines.energy.insert(incubator, true);
        let inv = game.wos[&incubator].linked_inventory;
        game.add_new_item(game.data.group("Iron").unwrap(), inv, 0);
        game.time += 2.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&incubator].growth, 0.0, "nothing linked, nothing grows");
        let target = game.data.group("Bee1Larvae").unwrap();
        game.wos.get_mut(&incubator).unwrap().linked_groups = Some(vec![target]);
        let mut steps = 0;
        while game.wos[&incubator].linked_groups != Some(vec![]) && steps < 100 {
            game.time += 2.0;
            crate::machines::tick(&mut game);
            steps += 1;
        }
        // The converter runs right after the grower in the same frame.
        assert_eq!(steps, (0.85f32 * 60.0 / 2.0).ceil() as i32);
        let content: Vec<usize> = game.inventories[&inv].items.iter().map(|i| game.wos[i].group).collect();
        assert_eq!(content, vec![target]);
        assert_eq!(game.wos[&incubator].growth, 0.0);
        assert_eq!(game.wos[&incubator].linked_groups, Some(vec![]));
    }

    #[test]
    fn vegetable_growers_fill_grow_and_replant_when_harvested() {
        let Some(mut game) = game() else { return };
        let grower = build(&mut game, "VegetableGrower1", [50.0, 5.0, 0.0]);
        crate::machines::tick(&mut game);
        let (seeds, second) = (game.wos[&grower].linked_inventory, game.wos[&grower].secondary_inventories[0]);
        assert!(game.inventories[&second].items.is_empty(), "no seed, nothing grows");
        game.add_new_item(game.data.group("Vegetable0Seed").unwrap(), seeds, 0);
        let vegetable = game.inventories[&second].items[0];
        let grown = game.data.group("Vegetable0Growable").unwrap();
        assert_eq!(game.wos[&vegetable].group, grown);
        assert_eq!(game.wos[&vegetable].position, [50.00476, 6.249, 0.03], "on the growing spot, as the game puts it");
        assert_eq!(game.wos[&vegetable].growth, 0.3f32 * 100.0);
        assert!(game.wos[&vegetable].net.is_some());
        // Its up axis is the soil's normal, which leans 7 degrees (measured: the game's crops).
        let up = rotate(game.wos[&vegetable].rotation, [0.0, 1.0, 0.0]);
        assert!((up[0] + 0.005).abs() < 2e-3 && (up[1] - 0.992).abs() < 2e-3 && (up[2] + 0.1265).abs() < 2e-3, "{up:?}");
        game.ext.machines.energy.insert(grower, true);
        for _ in 0..1200 {
            game.time += 1.0;
            crate::machines::tick(&mut game);
        }
        assert_eq!(game.wos[&vegetable].growth, 100.0);
        // A player grabs it: it leaves the grower and a new one starts.
        let backpack = game.create_inventory(10, 0, 0, &[], &[]);
        game.inventory_add(backpack, vegetable, true);
        crate::machines::grabbed(&mut game, vegetable);
        let next = game.inventories[&second].items.clone();
        assert_eq!(next.len(), 1);
        assert_ne!(next[0], vegetable);
        // A new kind of seed drops what still grows.
        let seed = game.inventories[&seeds].items[0];
        game.inventories_rpc(1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(seed), json!(seeds), json!(true)]));
        assert!(!game.wos.contains_key(&next[0]));
        assert!(game.inventories[&second].items.is_empty());
    }

    #[test]
    fn algae_grow_where_a_ray_down_meets_water() {
        let Some(mut game) = game() else { return };
        // A Prime lake at 57.23 m (machines: floating_machines_follow_the_rising_water).
        let planet = game.planet_hash();
        game.units.planet_mut(planet).unwrap().units.iter_mut().filter(|u| u.kind == crate::units::Kind::Terraformation).for_each(|u| u.total = 26.5e6);
        let wet = build(&mut game, "AlgaeGenerator1", [1420.0, 54.0, -800.0]);
        let dry = build(&mut game, "AlgaeGenerator1", [50.0, 5.0, 0.0]);
        crate::machines::tick(&mut game);
        let crops = |game: &Game, wo: i32| game.inventories[&game.wos[&wo].secondary_inventories[0]].items.clone();
        let (wet_crops, dry_crops) = (crops(&game, wet), crops(&game, dry));
        assert!(!wet_crops.is_empty(), "the generator on the lake fills its crop inventory");
        for c in wet_crops {
            let y = game.wos[&c].position[1];
            assert!((y - (57.23 - 0.1)).abs() < 0.02, "{y}: a tenth of a meter under the surface (downValue)");
        }
        assert!(dry_crops.is_empty(), "no water under the ring, no spot, no crop (CreateNewGrowable gives up)");
    }

    /// Hot update from 0.6.0 to 0.6.1 failed with "missing field water_only": the snapshot of
    /// the release before had no such field. What the tables give is not in a snapshot, and a
    /// restored algae generator still knows it needs water.
    #[test]
    fn a_restored_algae_generator_still_needs_water() {
        let Some(mut game) = game() else { return };
        let wo = build(&mut game, "AlgaeGenerator1", [50.0, 5.0, 0.0]);
        let snapshot = crate::handoff::snapshot(&mut game).unwrap();
        assert!(!serde_json::to_string(&game).unwrap().contains("water_only"), "the flag is not part of the state: the tables give it");
        let water_only = |g: &Game| g.ext.machines.list.get(&wo).and_then(|m| m.harvestable.as_ref()).map(|h| h.water_only);
        assert_eq!(water_only(&game), Some(true));
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        let restored = crate::handoff::restore(net_data, data, spec, &snapshot, scenes).unwrap();
        assert_eq!(water_only(&restored), Some(true), "set again from the tables after the restore");
        // The vegetable grower on the same restore does not need water.
        let grower = build(&mut game, "VegetableGrower1", [60.0, 5.0, 0.0]);
        let snapshot = crate::handoff::snapshot(&mut game).unwrap();
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        let restored = crate::handoff::restore(net_data, data, spec, &snapshot, scenes).unwrap();
        assert_eq!(restored.ext.machines.list.get(&grower).and_then(|m| m.harvestable.as_ref()).map(|h| h.water_only), Some(false));
    }

    #[test]
    fn crops_of_growers_on_other_planets_stay_inactive() {
        let Some(mut game) = game() else { return };
        let grower = build(&mut game, "VegetableGrower1", [50.0, 5.0, 0.0]);
        let aqualis = crate::data::stable_hash("Aqualis");
        game.wos.get_mut(&grower).unwrap().planet = aqualis;
        crate::machines::tick(&mut game);
        let (seeds, second) = (game.wos[&grower].linked_inventory, game.wos[&grower].secondary_inventories[0]);
        game.add_new_item(game.data.group("Vegetable0Seed").unwrap(), seeds, 0);
        let vegetable = game.inventories[&second].items[0];
        assert_eq!(game.wos[&vegetable].planet, aqualis);
        assert_eq!(game.wos[&vegetable].position, [50.0, 5.0, 0.0], "an inactive grower spawns on itself");
        let net = game.wos[&vegetable].net.unwrap();
        let active = game.net.get(net, "GroupNetworkContainer", "_active");
        assert!(matches!(active, Some(crate::net::VarValue::Plain(b)) if *b == enc::bool(false)), "{active:?}");
    }

    #[test]
    fn crops_grown_while_away_move_onto_the_spot_when_the_session_arrives() {
        let Some(mut game) = game() else { return };
        let grower = build(&mut game, "VegetableGrower1", [50.0, 5.0, 0.0]);
        let aqualis = crate::data::stable_hash("Aqualis");
        game.wos.get_mut(&grower).unwrap().planet = aqualis;
        crate::machines::tick(&mut game);
        let (seeds, second) = (game.wos[&grower].linked_inventory, game.wos[&grower].secondary_inventories[0]);
        game.add_new_item(game.data.group("Vegetable0Seed").unwrap(), seeds, 0);
        let vegetable = game.inventories[&second].items[0];
        assert_eq!(game.wos[&vegetable].position, [50.0, 5.0, 0.0], "spawned on the inactive grower's pivot");
        crate::planets::switch_to_planet(&mut game, aqualis, 0, 0);
        assert_eq!(game.wos[&vegetable].position, [50.00476, 6.249, 0.03]);
        let net = game.wos[&vegetable].net.unwrap();
        assert_eq!(game.net.objects[&net].position, [50.00476, 6.249, 0.03], "late joiners see it there");
    }

    use crate::game::Args;
}
