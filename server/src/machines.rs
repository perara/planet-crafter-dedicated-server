//! Machines: the game components on buildings that act on the world over time (generators,
//! growers, crafters...), server side. Each placed world object gets the machine components its
//! prefab root carries, wired the way WorldObjectsHandler.InstantiateWorldObject wires them
//! (WorldObjectAssociated.SetWorldObject, then InventoryAssociated.SetInventory); their
//! coroutines are timers against game.time, their inventory delegates run from
//! Inventory.PropagateModification (inventory_changed).
//!
//! What the server cannot know without the game's physics is approximated, each marked
//! `ponytail:`: raycasts (spawn points of grown vegetables, water), overlap spheres (machines in
//! range use world object positions) and collider bounds. Ore veins under extractors are cast
//! against the vein colliders of the scene dump (veins.rs).

pub(crate) mod growers;
mod production;
mod veins;

use std::collections::HashMap;

use serde_json::json;

use crate::clock::GameTime;
use crate::doc::Doc;
use crate::game::{Args, Game};
use crate::model::WorldObject;
use crate::net::Target;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Machines {
    /// World object -> has energy (machines with a RequireEnergy component).
    pub energy: crate::idmap::IdMap<i32, bool>,
    /// The machine components of instantiated world objects, in instantiation order.
    list: crate::idmap::IdMap<i32, Machine>,
    /// Inventory -> the machine whose delegates listen to it (inventoryContentModified).
    listening: HashMap<i32, i32>,
    /// ActionGrabable.grabedEvent: grown item -> (harvesting machine, the item's network object).
    grab: crate::idmap::IdMap<i32, (i32, u64)>,
    /// Net::removed when tick last looked for gone grab items; None: look again (a new item).
    #[serde(skip)]
    grab_seen: Option<u64>,
    /// Machines whose Start (or a coroutine's first `yield return null`) is due next tick.
    starting: Vec<i32>,
    /// MachineAutoCrafter._instancedAutocrafters.
    autocrafters: i32,
    /// WorldObjectsHandler.GetHasInitiatedAllObjects.
    loaded: bool,
    /// The current planet's ore veins.
    veins: Option<veins::State>,
    /// tick's list of machines (kept to reuse its allocation).
    ids: Vec<i32>,
    /// No machine has anything due before this (a lower bound; every change to a machine
    /// resets it, and tick skips its walk until then).
    next_due: GameTime,
    /// `next_due(m)` of every machine, in list order, so that the walk reads a flat array and
    /// looks at a machine only when it is due or changed. Valid while `dues_ok`; a machine got
    /// through `get_mut` is in `dirty` (its place in the list) until its entry is made again.
    /// Adding or removing a machine makes the whole array again at the next walk.
    #[serde(skip)]
    dues: Vec<GameTime>,
    #[serde(skip)]
    dues_ok: bool,
    #[serde(skip)]
    dirty: Vec<usize>,
    /// Counts the machines added and removed (a walk in progress compares it).
    #[serde(skip)]
    structure: u64,
    /// The authorized and unauthorized item groups of a building's inventory (and whether it is
    /// equipment), per building group: worked out from the tables once.
    #[serde(skip)]
    authorized: crate::idmap::FxHashMap<usize, (Vec<usize>, Vec<usize>, bool)>,
}

impl Machines {
    /// A machine to change (its timers may move earlier: tick walks all machines again).
    fn get_mut(&mut self, wo: &i32) -> Option<&mut Machine> {
        self.next_due = GameTime::PAST;
        if self.dues_ok {
            self.dirty.extend(self.list.get_index_of(wo));
        }
        self.list.get_mut(wo)
    }

    /// A machine added (at the end of the list, or in place of the one of the same world object).
    fn insert(&mut self, wo: i32, m: Machine) {
        self.next_due = GameTime::PAST;
        self.structure += 1;
        let due = (self.dues_ok).then(|| due_of(&m));
        let (i, replaced) = self.list.insert_full(wo, m);
        if let Some(due) = due {
            if replaced.is_some() {
                self.dues[i] = due;
            } else {
                self.dues.push(due);
            }
        }
    }

    /// A machine taken out; whether there was one.
    fn remove(&mut self, wo: &i32) -> bool {
        let Some((i, _, _)) = self.list.shift_remove_full(wo) else { return false };
        self.structure += 1;
        if self.dues_ok {
            self.dues.remove(i);
            self.dirty.retain(|d| *d != i);
            for d in self.dirty.iter_mut().filter(|d| **d > i) {
                *d -= 1;
            }
        }
        true
    }

    /// The array of due times as the machines are now.
    fn refresh_dues(&mut self, next_due: impl Fn(&Machine) -> GameTime) {
        if self.dues_ok {
            let mut dirty = std::mem::take(&mut self.dirty);
            for i in dirty.drain(..) {
                if let Some((_, m)) = self.list.get_index(i) {
                    self.dues[i] = next_due(m);
                }
            }
            self.dirty = dirty;
        } else {
            self.dirty.clear();
            self.dues.clear();
            self.dues.extend(self.list.values().map(next_due));
            self.dues_ok = true;
        }
    }

    pub fn with_energy(&self) -> Vec<i32> {
        self.energy.keys().copied().collect()
    }
}

/// One world object's machine components. Nine in ten machines have none of the parts a building
/// can make (a container, a wall light): those are 48 bytes, and the parts (584) are made when the
/// first is set. They read through `Deref` (`m.generator`); the writes say what they mean:
/// `parts_mut` makes the parts, `existing_parts_mut` changes them if the machine has any.
struct Machine {
    /// The inventory InventoryAssociated.SetInventory gave the machines (0: none).
    linked: i32,
    /// MachineGrowerBase._growSpeedMultiplier (optimizers' growth fuses).
    grow_multiplier: f32,
    /// MachineGrowerBase components on the root: (counts for growth fuses).
    growers: Vec<bool>,
    destruct_if_full: bool,
    parts: Option<Box<Parts>>,
}

/// The components a machine may have. All of them none: no `Parts`.
struct Parts {
    generator: Option<production::Generator>,
    linked_grower: Option<growers::LinkedGrower>,
    converter: Option<GameTime>,
    vegetation: Option<growers::Vegetation>,
    harvestable: Option<growers::Harvestable>,
    autocrafter: Option<production::AutoCrafter>,
    disintegrator: Option<production::Disintegrator>,
    optimizer: Option<production::Optimizer>,
    /// WorldUnitGenerationViaInventory unit indices.
    unit_generation: Vec<&'static str>,
    /// WorldUnitMultiplierViaInventory: (unit, influencedByHunger).
    unit_multipliers: Vec<(&'static str, bool)>,
    /// MachineFloater on water: (updateWaterSurfaceInterval, next update).
    floater: Option<(f32, GameTime)>,
}

static NO_PARTS: Parts = Parts {
    generator: None,
    linked_grower: None,
    converter: None,
    vegetation: None,
    harvestable: None,
    autocrafter: None,
    disintegrator: None,
    optimizer: None,
    unit_generation: Vec::new(),
    unit_multipliers: Vec::new(),
    floater: None,
};

const _: () = assert!(std::mem::size_of::<Machine>() == 48);

impl std::ops::Deref for Machine {
    type Target = Parts;
    fn deref(&self) -> &Parts {
        self.parts.as_deref().unwrap_or(&NO_PARTS)
    }
}

impl Machine {
    fn new() -> Machine {
        Machine { linked: 0, grow_multiplier: 1.0, growers: Vec::new(), destruct_if_full: false, parts: None }
    }

    /// The parts to set one: made if the machine has none yet.
    fn parts_mut(&mut self) -> &mut Parts {
        self.parts.get_or_insert_with(|| {
            Box::new(Parts { generator: None, linked_grower: None, converter: None, vegetation: None, harvestable: None, autocrafter: None, disintegrator: None, optimizer: None, unit_generation: Vec::new(), unit_multipliers: Vec::new(), floater: None })
        })
    }

    /// The parts to change what the machine has; None for a machine without any (nothing is made).
    fn existing_parts_mut(&mut self) -> Option<&mut Parts> {
        self.parts.as_deref_mut()
    }
}

/// A snapshot's machine: the fields in the order they were written when they were the struct's
/// own (bincodec.rs writes a struct as its fields back to back).
#[derive(serde::Deserialize)]
struct Flat {
    linked: i32,
    grow_multiplier: f32,
    growers: Vec<bool>,
    generator: Option<production::Generator>,
    linked_grower: Option<growers::LinkedGrower>,
    converter: Option<GameTime>,
    vegetation: Option<growers::Vegetation>,
    harvestable: Option<growers::Harvestable>,
    autocrafter: Option<production::AutoCrafter>,
    disintegrator: Option<production::Disintegrator>,
    optimizer: Option<production::Optimizer>,
    destruct_if_full: bool,
    #[serde(with = "unit_names")]
    unit_generation: Vec<&'static str>,
    #[serde(with = "unit_names")]
    unit_multipliers: Vec<(&'static str, bool)>,
    floater: Option<(f32, GameTime)>,
}

struct Named<'a, T>(&'a T);

impl<T: unit_names::Names> serde::Serialize for Named<'_, T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        unit_names::serialize(self.0, s)
    }
}

impl serde::Serialize for Machine {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let p: &Parts = self;
        let mut t = s.serialize_struct("Machine", 15)?;
        t.serialize_field("linked", &self.linked)?;
        t.serialize_field("grow_multiplier", &self.grow_multiplier)?;
        t.serialize_field("growers", &self.growers)?;
        t.serialize_field("generator", &p.generator)?;
        t.serialize_field("linked_grower", &p.linked_grower)?;
        t.serialize_field("converter", &p.converter)?;
        t.serialize_field("vegetation", &p.vegetation)?;
        t.serialize_field("harvestable", &p.harvestable)?;
        t.serialize_field("autocrafter", &p.autocrafter)?;
        t.serialize_field("disintegrator", &p.disintegrator)?;
        t.serialize_field("optimizer", &p.optimizer)?;
        t.serialize_field("destruct_if_full", &self.destruct_if_full)?;
        t.serialize_field("unit_generation", &Named(&p.unit_generation))?;
        t.serialize_field("unit_multipliers", &Named(&p.unit_multipliers))?;
        t.serialize_field("floater", &p.floater)?;
        t.end()
    }
}

impl<'de> serde::Deserialize<'de> for Machine {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Machine, D::Error> {
        let f = Flat::deserialize(d)?;
        let parts = Parts {
            generator: f.generator,
            linked_grower: f.linked_grower,
            converter: f.converter,
            vegetation: f.vegetation,
            harvestable: f.harvestable,
            autocrafter: f.autocrafter,
            disintegrator: f.disintegrator,
            optimizer: f.optimizer,
            unit_generation: f.unit_generation,
            unit_multipliers: f.unit_multipliers,
            floater: f.floater,
        };
        let any = parts.generator.is_some()
            || parts.linked_grower.is_some()
            || parts.converter.is_some()
            || parts.vegetation.is_some()
            || parts.harvestable.is_some()
            || parts.autocrafter.is_some()
            || parts.disintegrator.is_some()
            || parts.optimizer.is_some()
            || !parts.unit_generation.is_empty()
            || !parts.unit_multipliers.is_empty()
            || parts.floater.is_some();
        Ok(Machine { linked: f.linked, grow_multiplier: f.grow_multiplier, growers: f.growers, destruct_if_full: f.destruct_if_full, parts: any.then(|| Box::new(parts)) })
    }
}

impl Machines {
    /// (machines, machines with a component), for the memory tests.
    #[cfg(test)]
    pub fn census(&self) -> (usize, usize) {
        (self.list.len(), self.list.values().filter(|m| m.parts.is_some()).count())
    }
}

/// How many machines have each component: where the machines' memory goes (bench::scale).
#[cfg(test)]
pub fn census(machines: &Machines) -> String {
    let n = machines.list.len();
    let count = |f: &dyn Fn(&Machine) -> bool| machines.list.values().filter(|m| f(m)).count();
    format!(
        "{n} machines ({} bytes each): generator {}, linked_grower {}, converter {}, vegetation {}, harvestable {}, autocrafter {}, disintegrator {}, optimizer {}, floater {}, growers {}, unit_generation {}, unit_multipliers {}",
        std::mem::size_of::<Machine>(),
        count(&|m| m.generator.is_some()),
        count(&|m| m.linked_grower.is_some()),
        count(&|m| m.converter.is_some()),
        count(&|m| m.vegetation.is_some()),
        count(&|m| m.harvestable.is_some()),
        count(&|m| m.autocrafter.is_some()),
        count(&|m| m.disintegrator.is_some()),
        count(&|m| m.optimizer.is_some()),
        count(&|m| m.floater.is_some()),
        count(&|m| !m.growers.is_empty()),
        count(&|m| !m.unit_generation.is_empty()),
        count(&|m| !m.unit_multipliers.is_empty()),
    )
}

/// DataConfig.WorldUnitType names world objects keep generation for.
const UNITS: [&str; 8] = crate::gamedata::UNIT_TYPES;

fn unit_name(name: &str) -> Option<&'static str> {
    UNITS.iter().copied().find(|u| *u == name).or(match name {
        "Terraformation" => Some("Terraformation"),
        "Biomass" => Some("Biomass"),
        _ => None,
    })
}

/// Unit names in a snapshot: back to the static names (an unknown one is dropped).
mod unit_names {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub trait Names: Sized {
        fn to_strings(&self) -> Vec<(String, bool)>;
        fn from_strings(v: Vec<(String, bool)>) -> Self;
    }
    impl Names for Vec<&'static str> {
        fn to_strings(&self) -> Vec<(String, bool)> {
            self.iter().map(|u| (u.to_string(), false)).collect()
        }
        fn from_strings(v: Vec<(String, bool)>) -> Self {
            v.into_iter().filter_map(|(u, _)| super::unit_name(&u)).collect()
        }
    }
    impl Names for Vec<(&'static str, bool)> {
        fn to_strings(&self) -> Vec<(String, bool)> {
            self.iter().map(|(u, h)| (u.to_string(), *h)).collect()
        }
        fn from_strings(v: Vec<(String, bool)>) -> Self {
            v.into_iter().filter_map(|(u, h)| super::unit_name(&u).map(|u| (u, h))).collect()
        }
    }
    pub fn serialize<T: Names, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        v.to_strings().serialize(s)
    }
    pub fn deserialize<'de, T: Names, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        Ok(T::from_strings(Deserialize::deserialize(d)?))
    }
}

/// Serialized fields of a group's prefab components of one type; `root` keeps those on the
/// prefab's root object only (GetComponent rather than GetComponentInChildren).
fn components<'a>(game: &'a Game, group: usize, ty: &str, root: bool) -> Vec<&'a Doc> {
    game.data.group_prefabs.components(&game.data.groups[group].id, ty, root)
}

/// The first such component on the prefab root.
fn root(game: &Game, group: usize, ty: &str) -> Option<Doc> {
    components(game, group, ty, true).first().map(|v| (*v).clone())
}

/// What a machine takes from the tables, set again from them after a restore. A snapshot
/// holds the values the release that wrote it derived from the tables, which the release that
/// restores it (of the same format) may read differently: they may not be kept. State (timers, what is
/// growing, the random sequence, what the rays found) is the snapshot's. A machine part that
/// keeps table values beside its state is listed here (docs/hot-update.md, "State"); Generator
/// (spawn_every, speed, groups) mixes both and is not refreshed.
pub fn refresh_from_tables(game: &mut Game) {
    let machines: Vec<i32> = game.ext.machines.list.keys().copied().collect();
    game.ext.machines.dues_ok = false;
    // What the tables say is a function of the group: read once for each group of a machine.
    type Roots = [Option<Doc>; 6];
    let mut roots: crate::idmap::FxHashMap<usize, Roots> = Default::default();
    for wo in machines {
        let Some(group) = game.wos.get(&wo).map(|w| w.group) else { continue };
        let [harvestable, vegetation, linked, autocrafter, disintegrator, optimizer] = roots.entry(group).or_insert_with(|| {
            let f = |ty: &str| root(game, group, ty);
            [f("MachineGrowerVegetationHarvestable"), f("MachineGrowerVegetationStatic"), f("MachineGrowerIfLinkedGroup"), f("MachineAutoCrafter"), f("MachineDisintegrator"), f("MachineOptimizer")]
        });
        let Some(m) = game.ext.machines.list.get_mut(&wo) else { continue };
        if let (Some(h), Some(f)) = (m.existing_parts_mut().and_then(|p| p.harvestable.as_mut()), &*harvestable) {
            h.refresh(f);
        }
        if let (Some(v), Some(f)) = (m.existing_parts_mut().and_then(|p| p.vegetation.as_mut()), &*vegetation) {
            v.refresh(f);
        }
        if let (Some(l), Some(f)) = (m.existing_parts_mut().and_then(|p| p.linked_grower.as_mut()), &*linked) {
            l.refresh(f);
        }
        if let (Some(a), Some(f)) = (m.existing_parts_mut().and_then(|p| p.autocrafter.as_mut()), &*autocrafter) {
            a.refresh(f);
        }
        if let (Some(d), Some(f)) = (m.existing_parts_mut().and_then(|p| p.disintegrator.as_mut()), &*disintegrator) {
            d.refresh(f);
        }
        if let (Some(o), Some(f)) = (m.existing_parts_mut().and_then(|p| p.optimizer.as_mut()), &*optimizer) {
            o.refresh(f);
        }
    }
}

/// GetComponent<MachineRocketBackAndForth>() (its trade and interplanetary subclasses).
fn is_rocket_platform(game: &Game, group: usize) -> bool {
    crate::rockets::PLATFORM_CLASSES.iter().any(|class| !components(game, group, class, true).is_empty())
}

fn f32_of(v: &Doc, key: &str) -> f32 {
    v[key].as_f64().unwrap_or(0.0) as f32
}

fn has_energy(game: &Game, wo: i32) -> bool {
    game.ext.machines.energy.get(&wo).copied().unwrap_or(false)
}

/// gameObject.activeInHierarchy: world objects of other planets are inactive.
fn is_active(game: &Game, wo: i32) -> bool {
    game.wos.get(&wo).is_some_and(|w| w.planet_hash() == game.planet_hash())
}

fn net_has(game: &Game, wo: i32, class: &str) -> bool {
    game.wos.get(&wo).and_then(|w| w.net).and_then(|n| game.net.objects.get(&n)).is_some_and(|o| o.behaviour(class).is_some())
}

fn items(game: &Game, inventory: i32) -> Vec<i32> {
    game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default()
}

fn is_full(game: &Game, inventory: i32) -> bool {
    game.inventories.get(&inventory).is_none_or(|i| i.is_full())
}

fn group_ref(game: &Game, group: usize, field: &str) -> Option<usize> {
    game.data.group_of_ref(game.data.groups[group].fields[field].as_str()?)
}

/// GroupsHandler.GetGroupsViaGameObject.
fn group_of_game_object(game: &Game, go: &str) -> Option<usize> {
    game.data.groups.iter().position(|g| g.fields["associatedGameObject"].as_str() == Some(go))
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
}

// ---- the server calling its own handlers (sender 0: the host, whose replies stay local) ----

/// InventoriesHandler.AddItemToInventory(group, inventory) from the server.
fn add_new(game: &mut Game, group: usize, inventory: i32) -> bool {
    game.add_new_item(group, inventory, 0).0
}

/// InventoriesHandler.RemoveItemFromInventory from the server (RemoveItemFromInventoryServerRpc).
fn remove_item(game: &mut Game, wo: i32, inventory: i32, destroy: bool) -> bool {
    let ok = game.wos.contains_key(&wo) && game.inventory_remove(inventory, wo);
    if ok {
        game.propagate(inventory, wo, false);
        if destroy {
            game.destroy_world_object(wo, false);
        }
        game.dirty_inventory(inventory, 0, false);
    }
    ok
}

/// InventoriesHandler.RemoveAndDestroyAllItemsFromInventory from the server.
fn remove_and_destroy_all(game: &mut Game, inventory: i32) {
    game.inventories_rpc(0, "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", &Args(vec![json!(inventory)]));
}

/// LinkedGroupsProxy.SetLinkedGroups from the server: the network list and the world object.
fn set_linked_groups(game: &mut Game, wo: i32, groups: &[usize]) {
    let hashes: Vec<i32> = groups.iter().map(|g| game.data.groups[*g].hash).collect();
    match game.wos.get(&wo).and_then(|w| w.net) {
        Some(net) if net_has(game, wo, "LinkedGroupsProxy") => crate::handlers::set_linked_groups(game, net, &hashes),
        _ => {
            if let Some(w) = game.wos.get_mut(&wo) {
                w.linked_groups = Some(groups.to_vec());
            }
        }
    }
}

/// WorldUnit.ForceResetValues of one unit of a planet (0: the current one).
fn force_reset(game: &mut Game, unit: &str, planet: i32) {
    let planet = if planet == 0 { game.planet_hash() } else { planet };
    let Some(kind) = crate::units::Kind::from_name(unit) else { return };
    if let Some(p) = game.units.planet_mut(planet) {
        p.units.iter_mut().filter(|u| u.kind == kind).for_each(|u| u.force_reset = true);
    }
}

// ---- hooks ------------------------------------------------------------------------------

pub fn init(game: &mut Game) {
    game.ext.machines.loaded = true;
    // MachineOptimizer.WaitBeforeInit: optimizers loaded with the world apply their fuses now.
    let optimizers: Vec<i32> = game.ext.machines.list.iter().filter(|(_, m)| m.optimizer.is_some()).map(|(k, _)| *k).collect();
    for wo in optimizers {
        production::change_multiplier(game, wo);
    }
}

pub fn rpc(game: &mut Game, client: u64, object: u64, _behaviour: u16, name: &str, a: &Args) -> bool {
    let wo = game.net.objects.get(&object).and_then(|o| o.wo);
    // The back-and-forth rocket platforms' growth is the rockets port's, an animal's the animals'.
    if name.starts_with("GrowthProxy.") && (wo.and_then(|w| game.wos.get(&w)).is_some_and(|w| is_rocket_platform(game, w.group)) || crate::animals::spawn::is_animal(game, object)) {
        return false;
    }
    match name {
        "GrowthProxy.GetCurrentGrowthServerRpc" => {
            let value = wo.map(|w| growers::growth(game, w)).unwrap_or(0.0);
            game.net.rpc(Target::Only(client), object, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(value)]);
        }
        "GrowthProxy.ResetCurrentGrowthServerRpc" => {
            if let Some(w) = wo {
                growers::reset_current_growth(game, w, a.f(0));
            }
        }
        "ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc" => {
            if let Some((state, growth)) = wo.and_then(|w| growers::outside_grower_state(game, w)) {
                game.net.rpc(Target::Only(client), object, "ConstructibleProxy.MachineOutsideGrowerUpdateStartClientRpc", &[json!(state), json!(growth)]);
            }
        }
        "ConstructibleProxy.StartCraftAnimationServerRpc" => {
            game.net.rpc(Target::Except(client), object, "ConstructibleProxy.StartCraftAnimationClientRpc", &[json!(a.i(0))]);
        }
        "ConstructibleProxy.RecycleContentServerRpc" => {
            if let Some(w) = wo {
                production::recycle_content(game, w, a.v3(0), a.v3(1));
            }
        }
        "GeneticTraitHandler.CreateNewTraitWorldObjectServerRpc" if game.others_inventory(client, a.i(1)) => {}
        "GeneticTraitHandler.CreateNewTraitWorldObjectServerRpc" => {
            let Some(group) = game.data.group(crate::genetics::TRAIT_GROUP) else { return true };
            let id = game.create_world_object(group, 0, true);
            if let (Some(w), Some(data)) = (game.wos.get_mut(&id), a.0.first()) {
                crate::genetics::set_trait_serialized(w, data);
            }
            if !game.add_existing_item(id, a.i(1), true, 0) {
                game.destroy_world_object(id, false);
            }
        }
        _ => return false,
    }
    true
}

/// A world object got its network object (InstantiateWorldObject / ConstructibleProxy spawn):
/// its machine components start as the game wires them.
pub fn on_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let (group, linked, position) = (w.group, w.linked_inventory, w.position);
    let mut m = Machine::new();
    // MachineGrowerBase and its subclasses on the root, in component order (growth fuses).
    for ty in game.data.group_prefabs.root_types(&game.data.groups[group].id) {
        match ty {
            "MachineGrowerBase" | "MachineGrowerVegetationStatic" | "MachineGrowerVegetationHarvestable" => m.growers.push(true),
            "MachineGrowerIfLinkedGroup" | "MachineFlockSpawner" => m.growers.push(false),
            _ => {}
        }
    }
    let now = game.time;
    // Awake.
    if let Some(f) = root(game, group, "MachineAutoCrafter") {
        game.ext.machines.autocrafters += 1;
        m.parts_mut().autocrafter = Some(production::AutoCrafter::new(&f, now, game.ext.machines.autocrafters));
    }
    if let Some(f) = root(game, group, "MachineGrowerIfLinkedGroup") {
        m.parts_mut().linked_grower = Some(growers::LinkedGrower::new(&f, now));
    }
    if root(game, group, "MachineConvertRecipe").is_some() {
        m.parts_mut().converter = Some(now);
    }
    let harvestable = root(game, group, "MachineGrowerVegetationHarvestable");
    // RequireEnergy (WorldObjectAssociated.SetWorldObject -> AddToRequester, which checks the
    // machine's energy status at once: underwater it gets none).
    if root(game, group, "RequireEnergy").is_some() {
        let planet = game.wos[&wo].planet_hash();
        let enough = crate::energy::has_enough(game, planet);
        // Loading: one set of water boxes for every machine (see Game::load_water).
        let mut shared = game.load_water.take();
        let runs = crate::energy::check_energy_status(game, wo, enough, shared.as_mut().unwrap_or(&mut Default::default()));
        game.load_water = shared;
        game.ext.machines.energy.insert(wo, runs);
    }
    // MachineGrowerVegetationStatic.SetWorldObjectForGrower.
    if let Some(f) = root(game, group, "MachineGrowerVegetationStatic") {
        m.parts_mut().vegetation = Some(growers::Vegetation::new(&f, wo, now));
    }
    m.destruct_if_full = root(game, group, "MachineDestructInventoryIfFull").is_some();
    // MachineFloater.Start: those not marked notOnWater follow the water surface. One on a child
    // moves the world object of its parent too (GetComponentInParent<WorldObjectAssociated>).
    let floaters = components(game, group, "MachineFloater", false);
    let interval = floaters.iter().filter(|f| f["notOnWater"].as_bool() != Some(true)).map(|f| f32_of(f, "updateWaterSurfaceInterval")).find(|i| *i > 0.0);
    if let Some(i) = interval {
        m.parts_mut().floater = Some((i, now + i));
    }
    if let Some(f) = harvestable {
        m.parts_mut().harvestable = Some(growers::Harvestable::new(&f));
    }
    // InventoryAssociated.SetInventory.
    if linked != 0 && game.inventories.contains_key(&linked) {
        m.linked = linked;
        if let Some(f) = root(game, group, "InventoryAuthorizedGroups") {
            set_authorized_groups(game, linked, group, &f);
        }
        if let Some(f) = root(game, group, "MachineGenerator") {
            // TryToCastRay casts only while the machine is active: one on another planet keeps
            // its saved linked groups until a travel brings it in (production::mining_rays).
            let cast = is_active(game, wo);
            if cast {
                veins::cast(game, wo, &f);
            }
            m.parts_mut().generator = Some(production::Generator::new(game, &f, wo, now, cast));
        }
        if let Some(f) = root(game, group, "MachineDisintegrator") {
            m.parts_mut().disintegrator = Some(production::Disintegrator::new(&f, now));
        }
        if let Some(f) = root(game, group, "MachineOptimizer") {
            m.parts_mut().optimizer = Some(production::Optimizer::new(&f));
        }
        for f in components(game, group, "WorldUnitGenerationViaInventory", true) {
            if let Some(unit) = f["worldUnitType"].as_str().and_then(unit_name) {
                m.parts_mut().unit_generation.push(unit);
            }
        }
        // The space multipliers' WorldUnitMultiplierViaInventory is the rockets port's.
        let space = game.data.groups[group].id.starts_with("SpaceMultiplier");
        for f in components(game, group, "WorldUnitMultiplierViaInventory", true).into_iter().filter(|_| !space) {
            if let Some(u) = f["worldUnitType"].as_str().and_then(unit_name) {
                m.parts_mut().unit_multipliers.push((u, f["influencedByHunger"].as_bool().unwrap_or(false)));
            }
        }
        game.ext.machines.listening.insert(linked, wo);
    }
    let (has_vegetation, has_optimizer, has_harvestable) = (m.vegetation.is_some(), m.optimizer.is_some(), m.harvestable.is_some());
    let (unit_generation, unit_multipliers) = (m.unit_generation.clone(), m.unit_multipliers.clone());
    let destruct = m.destruct_if_full && m.linked != 0;
    let wants_start = has_harvestable;
    let grows = m.growers.iter().any(|g| *g);
    game.ext.machines.next_due = GameTime::PAST;
    game.ext.machines.insert(wo, m);
    // Rules: growth speed, as a growth fuse would add it.
    if grows && game.rules.speed.growth != 1.0 {
        production::add_growth_speed(game, wo, game.rules.speed.growth - 1.0);
    }
    if linked != 0 && game.ext.machines.listening.get(&linked) == Some(&wo) {
        if has_vegetation {
            growers::vegetation_inventory_set(game, wo);
        }
        if destruct {
            production::try_to_clean(game, wo);
        }
        if has_optimizer && game.ext.machines.loaded {
            production::change_multiplier(game, wo);
        }
        for unit in unit_generation {
            unit_generation_changed(game, wo, unit);
        }
        for (unit, hunger) in unit_multipliers {
            let planet = game.wos.get(&wo).map(|w| w.planet_hash()).unwrap_or(0);
            for item in items(game, linked) {
                update_generation_multiplier(game, wo, unit, hunger, item, true, 1.0);
            }
            force_reset(game, unit, planet);
        }
    }
    if wants_start {
        game.ext.machines.starting.push(wo);
    }
    // ConstructibleProxy.OnNetworkSpawn: WorldObjectsHandler.onConstructibleInstantiated.
    if game.ext.machines.loaded && net_has(game, wo, "ConstructibleProxy") && game.data.groups[group].constructible {
        production::constructible_instantiated(game, wo, position);
    }
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    let machines = &mut game.ext.machines;
    machines.energy.swap_remove(&wo.id);
    if machines.remove(&wo.id) {
        machines.listening.retain(|_, m| *m != wo.id);
    }
    // The plants whose items were destroyed with their game objects left (objects.rs destroy_world_object)
    // are the machine's children: they go with it.
    let orphans: Vec<u64> = game.ext.machines.grab.iter().filter(|(item, (machine, _))| *machine == wo.id && !game.wos.contains_key(item)).map(|(_, (_, net))| *net).collect();
    for net in orphans {
        game.net.despawn(net);
    }
    // ConstructibleProxy.OnDestroy: WorldObjectsHandler.onConstructibleDestroyed.
    production::constructible_destroyed(game, wo.id);
}

/// RequireEnergy.ChangeComponentsStatuts: every machine kind of the object (growers, flock
/// spawners, generators, autocrafters, disintegrators) runs only while this is true.
pub fn set_has_energy(game: &mut Game, wo: i32, enough: bool) {
    if let Some(e) = game.ext.machines.energy.get_mut(&wo) {
        *e = enough;
    }
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    // InventoryDestroyWhenEmpty (death canisters).
    if let Some(owner) = game.ext.destroy_when_empty.get(&inventory).copied() {
        if game.inventories.get(&inventory).is_some_and(|i| i.items.is_empty()) {
            game.ext.destroy_when_empty.remove(&inventory);
            game.destroy_world_object(owner, true);
        }
    }
    let Some(machine) = game.ext.machines.listening.get(&inventory).copied() else { return };
    let Some(m) = game.ext.machines.list.get(&machine) else { return };
    if m.linked == inventory {
        // The delegates in the order InventoryAssociated.SetInventory registered them.
        let (vegetation, harvestable, destruct, optimizer) = (m.vegetation.is_some(), m.harvestable.is_some(), m.destruct_if_full, m.optimizer.is_some());
        let (unit_generation, unit_multipliers) = (m.unit_generation.clone(), m.unit_multipliers.clone());
        if vegetation {
            growers::vegetation_inventory_modified(game, machine, added);
        }
        if harvestable {
            growers::seed_inventory_modified(game, machine, true, added);
        }
        if destruct {
            production::try_to_clean(game, machine);
        }
        if optimizer {
            production::change_multiplier(game, machine);
            production::optimizer_update(game, machine);
        }
        for unit in unit_generation {
            unit_generation_changed(game, machine, unit);
        }
        for (unit, hunger) in unit_multipliers {
            update_generation_multiplier(game, machine, unit, hunger, wo, added, 1.0);
            let planet = game.wos.get(&machine).map(|w| w.planet_hash()).unwrap_or(0);
            force_reset(game, unit, planet);
        }
    }
    if game.ext.machines.list.get(&machine).and_then(|m| m.harvestable.as_ref()).and_then(|h| h.second) == Some(inventory) {
        growers::second_inventory_modified(game, machine, wo, added);
    }
}

impl Machines {
    /// Whether the item is a plant a grower holds for harvest (see `grabbed`).
    pub fn grabbable(&self, wo: i32) -> bool {
        self.grower_of(wo).is_some()
    }

    /// The grower that holds the plant.
    pub fn grower_of(&self, wo: i32) -> Option<i32> {
        self.grab.get(&wo).map(|(machine, _)| *machine)
    }
}

/// ActionGrabable.grabedEvent of a picked-up object.
pub fn grabbed(game: &mut Game, wo: i32) {
    if let Some((machine, _)) = game.ext.machines.grab.shift_remove(&wo) {
        growers::vegetable_grabbed(game, machine, wo);
    }
}

pub fn scene_object_removed(_game: &mut Game, _wo: i32) {}

/// Rules: the growth speed changed by `add`, for every grower there is.
pub fn add_growth_speed_to_all(game: &mut Game, add: f32) {
    let growers: Vec<i32> = game.ext.machines.list.iter().filter(|(_, m)| m.growers.iter().any(|g| *g)).map(|(wo, _)| *wo).collect();
    for wo in growers {
        production::add_growth_speed(game, wo, add);
    }
}

/// Rules: a joining client hears the growth speed of every grower (its own copies start at 1).
pub fn client_approved(game: &mut Game, client: u64) {
    let add = game.rules.speed.growth - 1.0;
    if add == 0.0 {
        return;
    }
    let growers: Vec<(u64, usize)> = game.ext.machines.list.iter().filter(|(_, m)| m.growers.iter().any(|g| *g)).filter_map(|(wo, m)| Some((game.wos.get(wo)?.net?, m.growers.len()))).collect();
    for (net, count) in growers {
        for _ in 0..count {
            game.net.rpc(Target::Only(client), net, "ConstructibleProxy.SetGrowerSpeedMultiplierClientRpc", &[json!(add)]);
        }
    }
}

/// The session arrived on another planet: its machines' game objects become active (OnEnable).
pub fn planet_loaded(game: &mut Game) {
    let enabled: Vec<i32> = game.ext.machines.list.iter().filter(|(wo, m)| m.harvestable.is_some() && is_active(game, **wo)).map(|(wo, _)| *wo).collect();
    for wo in enabled {
        growers::harvestable_enabled(game, wo);
    }
}

/// The earliest time `tick` has something to do for this machine.
fn due_of(m: &Machine) -> GameTime {
    production::next_due(m).min(growers::next_due(m)).min(m.floater.map_or(GameTime::NEVER, |(_, due)| due))
}

pub fn tick(game: &mut Game) {
    veins::ensure(game);
    // Start / `yield return null`: the frame after instantiation.
    for wo in std::mem::take(&mut game.ext.machines.starting) {
        if !game.ext.machines.list.contains_key(&wo) {
            continue;
        }
        if game.ext.machines.list[&wo].harvestable.is_some() {
            growers::harvestable_init(game, wo);
        }
    }
    // ActionGrabable.OnDestroy fires grabedEvent when the item's game object goes.
    // Only looked for when an object went since the last look, or an item came.
    if game.ext.machines.grab_seen != Some(game.net.removed) {
        // Set first: what the loop below removes or grows is looked for again next tick.
        game.ext.machines.grab_seen = Some(game.net.removed);
        let gone: Vec<i32> = game.ext.machines.grab.iter().filter(|(_, (_, net))| !game.net.objects.contains_key(net)).map(|(k, _)| *k).collect();
        for item in gone {
            grabbed(game, item);
        }
    }
    let now = game.time;
    if now < game.ext.machines.next_due {
        return;
    }
    let next_due = due_of;
    let mut earliest = GameTime::NEVER;
    game.ext.machines.next_due = GameTime::NEVER;
    let mut ids = std::mem::take(&mut game.ext.machines.ids);
    ids.clear();
    ids.extend(game.ext.machines.list.keys());
    game.ext.machines.refresh_dues(next_due);
    let structure = game.ext.machines.structure;
    let mut k = 0;
    while k < ids.len() {
        // Machines nothing has changed, nothing due: the array says it (the common case, in one run).
        // (Not once a machine was added or removed in this walk: the ids are the list before.)
        let m = &game.ext.machines;
        if m.dues_ok && m.structure == structure && m.dirty.is_empty() {
            let rest = &m.dues[k..];
            let stop = rest.iter().position(|d| now >= *d).unwrap_or(rest.len());
            earliest = rest[..stop].iter().fold(earliest, |e, d| e.min(*d));
            k += stop;
            if k >= ids.len() {
                break;
            }
        }
        let wo = ids[k];
        // Nothing due at its turn: the ticks below would do nothing.
        let due = if game.ext.machines.dues_ok && game.ext.machines.structure == structure {
            game.ext.machines.refresh_dues(next_due);
            game.ext.machines.dues[k]
        } else {
            match game.ext.machines.list.get(&wo).map(next_due) {
                Some(due) => due,
                None => {
                    k += 1;
                    continue;
                }
            }
        };
        k += 1;
        if now < due {
            earliest = earliest.min(due);
            continue;
        }
        production::tick(game, wo, now);
        growers::tick(game, wo, now);
        if let Some((interval, due)) = game.ext.machines.list.get(&wo).and_then(|m| m.floater) {
            if now >= due {
                game.ext.machines.get_mut(&wo).unwrap().parts_mut().floater = Some((interval, now + interval));
                float_on_water(game, wo);
            }
        }
    }
    game.ext.machines.ids = ids;
    // Changed while walking (each machine that ran): walk again next tick.
    game.ext.machines.next_due = game.ext.machines.next_due.min(earliest);
}

/// MachineFloater.SetReferencePositionOnSurface: the world object of a floating machine goes to
/// the water surface under it, a quarter meter down, with its rotation reset. Each client's own
/// MachineFloater moves what it shows; the network object's position is what a late joiner gets.
/// Runs while the machine's planet is loaded (its game object is active).
// ponytail: the cast starts at the world object's position, not at a floater child's own offset.
fn float_on_water(game: &mut Game, wo: i32) {
    if !is_active(game, wo) {
        return;
    }
    let p = game.wos[&wo].position;
    let Some(surface) = crate::water::surface_below(game, [p[0], p[1] + 5.0, p[2]], 105.0) else { return };
    let position = [p[0], surface - 0.25, p[2]];
    let w = game.wos.get_mut(&wo).unwrap();
    w.position = position;
    w.rotation = [0.0, 0.0, 0.0, 1.0];
    if let Some(net) = w.net {
        game.net.move_object(net, position, None);
    }
}

// ---- inventory listeners ----------------------------------------------------------------

/// InventoryAssociated.SetInventory: InventoryAuthorizedGroups on the inventory.
fn set_authorized_groups(game: &mut Game, inventory: i32, group: usize, f: &Doc) {
    // The same for every building of a group: worked out once (the tables do not change).
    if !game.ext.machines.authorized.contains_key(&group) {
        let groups = |categories: &str, sub_categories: &str| -> Vec<usize> {
            let list = |k: &str| f[k].as_array().into_iter().flatten().filter_map(Doc::as_str).map(str::to_string).collect::<Vec<_>>();
            let mut out: Vec<usize> = Vec::new();
            let items = || game.data.groups.iter().enumerate().filter(|(_, g)| !g.constructible);
            for c in list(categories) {
                out.extend(items().filter(|(i, g)| g.str("itemCategory") == c && !out.contains(i)).map(|(i, _)| i).collect::<Vec<_>>());
            }
            for c in list(sub_categories) {
                out.extend(items().filter(|(i, g)| g.str("itemSubCategory") == c && !out.contains(i)).map(|(i, _)| i).collect::<Vec<_>>());
            }
            out
        };
        let authorized = groups("authorizedItemCategories", "authorizedItemSubCategories");
        let unauthorized = groups("unauthorizedItemCategories", "unauthorizedItemSubCategories");
        let equipment = authorized.first().is_some_and(|g| game.data.groups[*g].str("itemCategory") == "Equipment");
        game.ext.machines.authorized.insert(group, (authorized, unauthorized, equipment));
    }
    let (authorized, unauthorized, equipment) = game.ext.machines.authorized[&group].clone();
    if let Some(inv) = game.inventories.get_mut(&inventory) {
        inv.authorized = authorized;
        inv.unauthorized = unauthorized;
        inv.is_equipment |= equipment;
    }
}

/// WorldUnitGenerationViaInventory.OnInventoryModified.
fn unit_generation_changed(game: &mut Game, wo: i32, unit: &str) {
    let Some(index) = crate::model::unit_index(unit) else { return };
    let linked = game.ext.machines.list.get(&wo).map(|m| m.linked).unwrap_or(0);
    let empty = game.inventories.get(&linked).is_none_or(|i| i.items.is_empty());
    let Some(w) = game.wos.get_mut(&wo) else { return };
    w.unit_generation[index] = if empty { 0.0 } else { game.data.groups[w.group].units[index] };
    force_reset(game, unit, 0);
}

/// WorldUnitMultiplierViaInventory.UpdateGenerationMultiplier.
fn update_generation_multiplier(game: &mut Game, wo: i32, unit: &str, hunger: bool, item: i32, added: bool, multiplier: f32) {
    // The game's world objects keep no multiplier for other units (it throws there).
    let Some(index) = crate::model::unit_index(unit) else { return };
    let Some(i) = game.wos.get(&item) else { return };
    let mut num = game.data.groups[i.group].units[index] / 100.0 * if added { 1.0 } else { -1.0 } * multiplier;
    if hunger {
        let i = game.wos.get_mut(&item).unwrap();
        if added {
            if i.hunger < 0.0 {
                num *= 0.5;
            }
            i.unit_multiplier[index] = num;
        } else {
            num = -i.unit_multiplier[index];
            i.unit_multiplier[index] = 0.0;
        }
    }
    if let Some(w) = game.wos.get_mut(&wo) {
        w.unit_multiplier[index] += num;
    }
}

// ---- ActionDeconstructible.CheckDeconstructOnServer ---------------------------------------

/// DeconstructStatus.
const OK: i32 = 0;
const SOMETHING_INSIDE: i32 = 1;
pub(crate) const INVENTORY_NOT_EMPTY: i32 = 2;

/// GroupNetworkBase.DeconstructServerRpc: GetComponentInChildren<ActionDeconstructible>(); without
/// one (wreck safes, effigies, space multipliers, Warden objects) the game does nothing at all.
pub fn deconstructible(game: &Game, wo: i32) -> bool {
    game.wos.get(&wo).is_some_and(|w| !components(game, w.group, "ActionDeconstructible", false).is_empty())
}

/// ActionDeconstructible.CheckDeconstructOnServer: 0 Ok, 1 SomethingInside, 2 InventoryNotEmpty.
pub fn deconstruct_status(game: &Game, wo: i32) -> i32 {
    let Some(w) = game.wos.get(&wo) else { return OK };
    // GroupNetworkBase.DeconstructServerRpc: GetComponentInChildren<ActionDeconstructible>().
    let Some(action) = components(game, w.group, "ActionDeconstructible", false).first().map(|v| (*v).clone()) else { return OK };
    if !something_inside_check(game, w, &action) {
        return SOMETHING_INSIDE;
    }
    let allow_full = action["canDeconstructIfContainerNotEmpty"].as_bool().unwrap_or(false);
    // InstantiateWorldObject adds an InventoryAssociated to anything with a linked inventory;
    // one that never got its inventory looks up id -1, which does not exist.
    let associated = !components(game, w.group, "InventoryAssociated", true).is_empty() || w.linked_inventory != 0;
    if !allow_full && associated {
        if game.inventories.get(&w.linked_inventory).is_none_or(|i| !i.items.is_empty()) {
            return INVENTORY_NOT_EMPTY;
        }
        if w.secondary_inventories.iter().any(|i| game.inventories.get(i).is_some_and(|i| !i.items.is_empty())) {
            return INVENTORY_NOT_EMPTY;
        }
    }
    OK
}

/// CheckSomethingInsideIfNeeded: no other building (but base building parts) inside the
/// detection collider, and no underground panels other than plain walls and floors.
fn something_inside_check(game: &Game, w: &WorldObject, action: &Doc) -> bool {
    let mut result = true;
    if action["checkInsideBeforeDeconstruction"].as_bool() == Some(true) {
        if let Some(bounds) = inside_bounds(game, w) {
            for id in &game.constructed {
                let Some(other) = game.wos.get(id) else { continue };
                let g = &game.data.groups[other.group];
                if other.id != w.id && bounds.contains(other.position) && g.constructible && g.str("groupCategory") != "BaseBuilding" {
                    result = false;
                }
            }
        }
    }
    if action["checkPanelsBeforeDeconstruction"].as_bool() == Some(true) {
        if w.panels.as_ref().is_some_and(|p| p.iter().any(|id| ![1, 7, 5].contains(id))) {
            result = false;
        }
    }
    result
}

/// An axis-aligned box in world space (UnityEngine.Bounds).
#[derive(Debug, PartialEq)]
struct Bounds {
    min: [f32; 3],
    max: [f32; 3],
}

impl Bounds {
    fn contains(&self, p: [f32; 3]) -> bool {
        (0..3).all(|i| p[i] >= self.min[i] && p[i] <= self.max[i])
    }

    /// The world bounds of a local box (center, half extents) on a transform.
    fn of_box(position: [f32; 3], rotation: [f32; 4], center: [f32; 3], half: [f32; 3]) -> Bounds {
        let c = growers::rotate(rotation, center);
        let mut extent = [0.0f32; 3];
        for axis in 0..3 {
            let mut unit = [0.0; 3];
            unit[axis] = half[axis];
            let r = growers::rotate(rotation, unit);
            (0..3).for_each(|i| extent[i] += r[i].abs());
        }
        Bounds { min: [0, 1, 2].map(|i| position[i] + c[i] - extent[i]), max: [0, 1, 2].map(|i| position[i] + c[i] + extent[i]) }
    }
}

/// The deconstruction detection volume of the buildings that check what is inside them.
// ponytail: the game uses collider (or renderer) bounds, which the data dump does not carry.
// These boxes (local min, max) were measured on the game (parity sweep, `sweep.py bounds`): a
// container placed further and further out from an unrotated building, along each axis both ways
// (1 m steps, then 1/16 m), until the building could be deconstructed, with everything near the
// host kept active (PerformancerDisableIfFarAway hides a volume further than 20 to 120 m from the
// host's player, which shrinks it). Each edge is the last offset inside plus 1/32 m; a floor
// between two heights is its middle. Biodome2 and ButterflyDome1 have a volume of a few
// decimetres around the pivot and skip the check. Upgrade: dump each ActionDeconstructible's
// bounds from the game.
fn inside_bounds(game: &Game, w: &WorldObject) -> Option<Bounds> {
    const POD: ([f32; 3], [f32; 3]) = ([-4.03, -0.02, -3.84], [4.03, 6.03, 4.16]);
    const BIOLAB: ([f32; 3], [f32; 3]) = ([-11.53, 0.71, -0.28], [3.59, 5.28, 14.47]);
    // 6.28 is the dome's measured height, not TAU.
    #[allow(clippy::approx_constant)]
    const DOME: ([f32; 3], [f32; 3]) = ([-8.66, 0.59, -8.72], [8.97, 6.28, 8.84]);
    const BOXES: &[(&str, ([f32; 3], [f32; 3]))] = &[
        ("pod", POD),
        ("podAngle", POD),
        ("PodUnderground", ([-4.03, -17.03, -3.84], [4.03, 2.47, 4.16])),
        ("InteriorStairs1", ([-4.03, -0.02, -3.84], [4.03, 12.03, 12.16])),
        ("Pod4x", BIOLAB),
        ("Biolab", BIOLAB),
        ("Pod9xA", ([-12.03, -0.02, -12.03], [12.03, 6.03, 12.03])),
        ("Pod9xB", DOME),
        ("Pod9xC", ([-12.53, 0.71, -10.84], [10.47, 5.28, 12.09])),
        ("biodome", DOME),
        ("Aquarium2", ([-14.47, -1.22, -9.47], [7.66, 6.91, 9.66])),
        ("VegetableGrower1", ([-0.66, -0.03, -0.59], [0.59, 2.78, 0.72])),
        ("VegetableGrower2", ([-0.66, -0.03, -0.59], [0.59, 2.78, 0.72])),
    ];
    let id = game.data.groups[w.group].id.as_str();
    let (_, (min, max)) = BOXES.iter().find(|(g, _)| *g == id)?;
    let center = [0, 1, 2].map(|i| (min[i] + max[i]) / 2.0);
    let half = [0, 1, 2].map(|i| (max[i] - min[i]) / 2.0);
    Some(Bounds::of_box(w.position, w.rotation, center, half))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;

    /// A server on an empty Prime save, with every placed object's machines wired.
    pub fn game() -> Option<Game> {
        let mut save = crate::save::SaveFile::empty();
        save.settings = json!({"planetId": "Prime"});
        save.world = json!({});
        save.stats = json!({});
        save.planets = vec![json!({"planetId": "Prime"})];
        crate::game::test_game(save)
    }

    /// Builds a group at a position the way CreateAndInstantiateWorldObjectServerRpc does.
    pub fn build(game: &mut Game, group: &str, position: [f32; 3]) -> i32 {
        let g = game.data.group(group).unwrap();
        let id = game.new_world_object_id();
        game.create_world_object(g, id, true);
        let planet = game.planet_hash();
        let w = game.wos.get_mut(&id).unwrap();
        w.position = position;
        w.rotation = [0.0, 0.0, 0.0, 1.0];
        w.planet = planet;
        game.instantiate(id, false, 0);
        id
    }

    /// A restore sets what machines take from the tables again and keeps their state: a
    /// snapshot holds the values the release that wrote it derived (here, every one changed),
    /// and the restored game has this release's, the snapshot's timers and all.
    #[test]
    fn a_restore_sets_the_table_values_again_and_keeps_the_state() {
        let Some(mut game) = game() else { return };
        let groups: Vec<String> = game.data.groups.iter().filter(|g| g.constructible && g.has_prefab && g.id != "ProceduralInstanceGenerator").map(|g| g.id.clone()).collect();
        for (i, group) in groups.iter().enumerate() {
            build(&mut game, group, [300.0 + (i % 17) as f32 * 30.0, 20.0, 300.0 + (i / 17) as f32 * 30.0]);
        }
        for _ in 0..30 {
            game.time += 1.0;
            tick(&mut game);
        }
        let text = crate::handoff::snapshot(&mut game).unwrap();
        // The state of the game as JSON, with the table values changed, is another release's.
        let mut state: Value = serde_json::to_value(&game).unwrap();
        let mut parts = std::collections::BTreeMap::<&str, usize>::new();
        let wrong = |v: &mut Value, keys: &[&str]| {
            for k in keys {
                v[*k] = match &v[*k] {
                    Value::Array(_) => json!(["Gone"]),
                    Value::Bool(b) => json!(!b),
                    Value::Null => Value::Null,
                    n if n.is_u64() || n.is_i64() => json!(12345),
                    _ => json!(12345.5),
                };
            }
        };
        for m in state["ext"]["machines"]["list"].as_object_mut().unwrap().values_mut() {
            for (part, keys) in [
                ("harvestable", &["interval", "grow_speed", "min_growth", "min_radius", "radius", "spawn_on_this", "down_value", "things"][..]),
                ("vegetation", &["grow_speed", "interval", "min_growth", "original"]),
                ("linked_grower", &["rate"]),
                ("autocrafter", &["period", "range"]),
                ("disintegrator", &["period", "give"]),
                ("optimizer", &["range", "max"]),
            ] {
                if !m[part].is_null() {
                    *parts.entry(part).or_default() += 1;
                    wrong(&mut m[part], keys);
                }
            }
        }
        for part in ["harvestable", "vegetation", "linked_grower", "autocrafter", "disintegrator", "optimizer"] {
            assert!(parts.get(part).copied().unwrap_or(0) > 0, "the world has a machine with a {part}: {parts:?}");
        }
        let mut earlier: Game = serde_json::from_value(state).unwrap();
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        (earlier.net_data, earlier.data, earlier.net.spec) = (net_data.clone(), data.clone(), spec.clone());
        let older = crate::handoff::snapshot(&mut earlier).unwrap();
        let restored = crate::handoff::restore(net_data, data, spec, &older, scenes).unwrap();
        crate::handoff::verify(&restored, &text).expect("the table's values are this release's, the state the snapshot's");
        assert!(crate::handoff::verify(&restored, &older).is_err(), "the older values were replaced");
    }

    #[test]
    fn floating_machines_follow_the_rising_water() {
        let Some(mut game) = game() else { return };
        // A Prime lake whose water rises from 45 m (TerraformStage4-Water) to 70 m (Lakes).
        let set_terraformation = |game: &mut Game, total: f64| {
            let planet = game.planet_hash();
            game.units.planet_mut(planet).unwrap().units.iter_mut().filter(|u| u.kind == crate::units::Kind::Terraformation).for_each(|u| u.total = total);
        };
        set_terraformation(&mut game, 26.5e6);
        let algae = build(&mut game, "AlgaeGenerator1", [1420.0, 54.0, -800.0]);
        let sunk = build(&mut game, "AlgaeGenerator1", [1430.0, 50.0, -800.0]);
        let lamp = build(&mut game, "OutsideLamp1", [1440.0, 54.0, -800.0]);
        game.time += 1.0;
        tick(&mut game);
        assert_eq!(game.wos[&algae].position[1], 54.0, "every 2 s");
        game.time += 1.0;
        tick(&mut game);
        let y = |game: &Game, wo: i32| game.wos[&wo].position[1];
        assert!((y(&game, algae) - 56.98).abs() < 1e-3, "{}: a quarter meter under the surface at 57.23", y(&game, algae));
        let net = game.wos[&algae].net.unwrap();
        assert_eq!(game.net.objects[&net].position, game.wos[&algae].position, "late joiners see it there");
        assert_eq!(y(&game, sunk), 50.0, "more than 5 m under: the ray starts inside the water");
        assert_eq!(y(&game, lamp), 54.0, "not a floater");
        set_terraformation(&mut game, 28.85e6);
        game.time += 2.0;
        tick(&mut game);
        assert!((y(&game, algae) - 58.235).abs() < 1e-3, "{}", y(&game, algae));
    }

    #[test]
    fn rotated_boxes_grow_their_world_bounds() {
        let b = Bounds::of_box([10.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [2.0, 1.0, 1.0]);
        assert_eq!(b, Bounds { min: [8.0, 0.0, -1.0], max: [12.0, 2.0, 1.0] });
        let quarter = crate::geometry::from_euler_y(90.0);
        let r = Bounds::of_box([0.0; 3], quarter, [0.0; 3], [2.0, 1.0, 1.0]);
        assert!((r.max[0] - 1.0).abs() < 1e-5 && (r.max[2] - 2.0).abs() < 1e-5);
        let diagonal = Bounds::of_box([0.0; 3], crate::geometry::from_euler_y(45.0), [0.0; 3], [1.0, 1.0, 1.0]);
        assert!((diagonal.max[0] - 2f32.sqrt()).abs() < 1e-5);
    }

    #[test]
    fn deconstruction_checks_what_is_inside() {
        let Some(mut game) = game() else { return };
        let pod = build(&mut game, "pod", [100.0, 10.0, 100.0]);
        assert_eq!(deconstruct_status(&game, pod), OK);
        let inside = build(&mut game, "Container1", [100.5, 10.5, 100.0]);
        assert_eq!(deconstruct_status(&game, pod), SOMETHING_INSIDE);
        game.destroy_world_object(inside, true);
        // The game's edges: inside at 3.94 m, outside at 4.13 m along x.
        let edge = build(&mut game, "Container1", [103.94, 11.0, 100.0]);
        assert_eq!(deconstruct_status(&game, pod), SOMETHING_INSIDE);
        game.destroy_world_object(edge, true);
        build(&mut game, "Container1", [104.13, 11.0, 100.0]);
        assert_eq!(deconstruct_status(&game, pod), OK);
        // A container with something in it.
        let chest = build(&mut game, "Container1", [200.0, 10.0, 100.0]);
        assert_eq!(deconstruct_status(&game, chest), OK);
        let inv = game.wos[&chest].linked_inventory;
        game.add_new_item(game.data.group("Iron").unwrap(), inv, 0);
        assert_eq!(deconstruct_status(&game, chest), INVENTORY_NOT_EMPTY);
    }

    /// The volumes measured on the game (sweep.py bounds): a Biolab and a Pod4x see what stands
    /// 0.75 m or more above their pivot, to 3.5 m east and 11.5 m west; the Aquarium2 reaches
    /// 14.4 m west but 7.6 m east; an InteriorStairs1 12 m up.
    #[test]
    fn deconstruction_volumes_are_the_measured_ones() {
        let Some(mut game) = game() else { return };
        let mut inside = |group: &str, at: [f32; 3], offset: [f32; 3]| {
            let b = build(&mut game, group, at);
            let c = build(&mut game, "Container1", [at[0] + offset[0], at[1] + offset[1], at[2] + offset[2]]);
            let status = deconstruct_status(&game, b);
            game.destroy_world_object(c, true);
            game.destroy_world_object(b, true);
            status == SOMETHING_INSIDE
        };
        assert!(inside("Biolab", [100.0, 10.0, 100.0], [-11.4, 1.0, 5.0]), "a Biolab has a volume");
        assert!(!inside("Biolab", [100.0, 10.0, 100.0], [-11.6, 1.0, 5.0]));
        assert!(!inside("Biolab", [100.0, 10.0, 100.0], [0.0, 0.5, 5.0]), "its floor is 0.7 m up");
        assert!(inside("Pod4x", [100.0, 10.0, 100.0], [3.5, 1.0, 5.0]));
        assert!(!inside("Pod4x", [100.0, 10.0, 100.0], [3.7, 1.0, 5.0]));
        assert!(inside("Aquarium2", [100.0, 10.0, 100.0], [-14.4, 1.0, 0.0]));
        assert!(!inside("Aquarium2", [100.0, 10.0, 100.0], [7.8, 1.0, 0.0]));
        assert!(inside("InteriorStairs1", [100.0, 10.0, 100.0], [0.0, 11.9, 0.0]));
        assert!(!inside("InteriorStairs1", [100.0, 10.0, 100.0], [0.0, 12.2, 0.0]));
    }

    #[test]
    fn inventories_get_their_authorized_groups() {
        let Some(mut game) = game() else { return };
        let iron = game.data.group("Iron").unwrap();
        let tube = build(&mut game, "Vegetube1", [10.0, 5.0, 0.0]);
        let inv = game.wos[&tube].linked_inventory;
        assert!(!game.add_new_item(iron, inv, 0).0, "vegetubes only take plant seeds");
        assert!(game.add_new_item(game.data.group("Seed0").unwrap(), inv, 0).0);
    }

    /// The ClientRpcs queued so far, by name.
    pub fn sent(game: &Game) -> Vec<(String, Vec<Value>)> {
        game.net
            .out
            .iter()
            .filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC)
            .filter_map(|(_, _, body)| {
                let mut r = crate::wire::Reader::new(body);
                let (_, _, method) = (r.packed_u64()?, r.packed_u16()?, r.packed_u32()?);
                Some((game.net.spec.get(method)?.name.clone(), game.net.spec.decode(method, r.rest())?))
            })
            .collect()
    }

    #[test]
    fn growth_proxies_answer_and_reset() {
        let Some(mut game) = game() else { return };
        let tube = build(&mut game, "Vegetube1", [10.0, 5.0, 0.0]);
        let net = game.wos[&tube].net.unwrap();
        game.wos.get_mut(&tube).unwrap().growth = 55.0;
        game.net.out.clear();
        rpc(&mut game, 1, net, 2, "GrowthProxy.GetCurrentGrowthServerRpc", &Args(vec![]));
        assert_eq!(sent(&game), vec![("GrowthProxy.SetCurrentGrowthClientRpc".to_string(), vec![json!(55.0)])]);
        rpc(&mut game, 1, net, 2, "GrowthProxy.ResetCurrentGrowthServerRpc", &Args(vec![json!(12.5)]));
        assert_eq!(game.wos[&tube].growth, 12.5);
        rpc(&mut game, 1, net, 0, "ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc", &Args(vec![]));
        let state = growers::outside_grower_state(&game, tube).unwrap().0;
        assert_eq!(sent(&game)[2], ("ConstructibleProxy.MachineOutsideGrowerUpdateStartClientRpc".to_string(), vec![json!(state), json!(12.5)]));
    }

    /// A crafting machine's animation goes to everyone but the crafter; a recycler breaks the
    /// item it holds when a client asks (here a full backpack: its content drops out first).
    #[test]
    fn crafting_animations_and_recycling_by_rpc() {
        use crate::testkit::*;
        let Some((mut game, _)) = world() else { return };
        join(&mut game, 2, "Bob");
        let (_, station) = place(&mut game, "CraftStation1", [570.0, 3.0, 500.0]);
        let iron = hash(&game, "Iron");
        game.net.out.clear();
        send_on(&mut game, 1, station, "ConstructibleProxy.StartCraftAnimationServerRpc", &[json!(iron)]);
        let s = crate::testkit::sent(&game, "ConstructibleProxy.StartCraftAnimationClientRpc");
        assert_eq!(s.into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Except(1), vec![json!(iron)])]);

        let (recycler, net) = place(&mut game, "RecyclingMachine", [580.0, 3.0, 500.0]);
        let inside = game.wos[&recycler].linked_inventory;
        let chest = give(&mut game, "Container1", inside);
        let chest_inventory = game.create_inventory(5, 0, chest, &[], &[]);
        let iron = give(&mut game, "Iron", chest_inventory);
        send_on(&mut game, 1, net, "ConstructibleProxy.RecycleContentServerRpc", &[json!([580.0, 4.0, 500.0]), json!([1.0, 1.0, 1.0])]);
        assert!(!game.wos.contains_key(&chest), "the holder goes");
        assert_eq!(game.wos[&iron].position, [580.0, 4.0, 500.0], "its content drops at the spot");
        assert!(game.inventories[&inside].items.is_empty());
    }

    #[test]
    fn genetic_traits_are_made_into_an_inventory() {
        let Some(mut game) = game() else { return };
        let inv = game.create_inventory(2, 0, 0, &[], &[]);
        let data = json!([2, [0.5, 0.25, 1.0, 1.0], 7]);
        rpc(&mut game, 1, 0, 0, "GeneticTraitHandler.CreateNewTraitWorldObjectServerRpc", &Args(vec![data.clone(), json!(inv)]));
        let id = game.inventories[&inv].items[0];
        let w = &game.wos[&id];
        assert_eq!((w.trait_type, w.trait_value, w.color), (2, 7, [0.5, 0.25, 1.0, 1.0]));
        let count = game.wos.len();
        rpc(&mut game, 1, 0, 0, "GeneticTraitHandler.CreateNewTraitWorldObjectServerRpc", &Args(vec![data, json!(-50)]));
        assert_eq!(game.wos.len(), count, "no inventory: the new trait is destroyed again");
    }

    #[test]
    fn unit_generation_follows_the_inventory() {
        let Some(mut game) = game() else { return };
        let tube = build(&mut game, "Vegetube1", [10.0, 5.0, 0.0]);
        let oxygen = crate::model::unit_index("Oxygen").unwrap();
        assert_eq!(game.wos[&tube].unit_generation[oxygen], 0.0);
        let inv = game.wos[&tube].linked_inventory;
        game.add_new_item(game.data.group("Seed0").unwrap(), inv, 0);
        let g = game.wos[&tube].group;
        assert_eq!(game.wos[&tube].unit_generation[oxygen], game.data.groups[g].units[oxygen]);
        let seed = game.wos[&tube].unit_multiplier[oxygen];
        let lirma = game.data.groups[game.data.group("Seed0").unwrap()].units[oxygen] / 100.0;
        assert_eq!(seed, lirma);
    }
}
