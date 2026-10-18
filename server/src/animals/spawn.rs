//! SpaceCraft.WorldAnimalSpawnerHandler and InventorySpawnContent, server side: the DNA
//! sequences in an animal shelter (or in a wild spawner a client reported) each become an animal
//! network object near players; hunger, feeders and growth; and the animals' server-side
//! behaviours (AIAnimalStateController, AnimalCustomizationController, GrowthProxy, PetProxy).

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};

use super::{distance2, nearest, player_distance2};
use crate::data::{BehaviourDef, VarDef};
use crate::game::{Args, Game};
use crate::gamedata::Layout;
use crate::geometry::MathRandom;
use crate::model::WorldObject;
use crate::net::{enc, NetObj, Target};
use crate::wire::Writer;

/// The AnimalList prefabs' network prefab hashes. gamedata.json lays out group prefabs only;
/// these come from the game's network prefab table (GET /api/manifest).
const PREFABS: [(&str, u32); 10] = [
    ("Sharky", 0x60be7ec3),
    ("AlienCat", 0x553c3117),
    ("Gazelle", 0x460320a1),
    ("LadyBug", 0xd39118a5),
    ("Buffalo", 0x1e125d4f),
    ("Horse", 0xee998591),
    ("Crab", 0x086edd87),
    ("Selenea", 0x884a38e6),
    ("Dog", 0x81939b5b),
    ("Lion", 0x3641b32a),
];
/// Spawn flags the game sends for animals (DontDestroyWithOwner, transform, scene).
const FLAGS: u16 = 232;
/// Behaviour indices on every animal prefab.
const AI: u16 = 0;
const TRANSFORM: u16 = 1;
const ANIMATOR: u16 = 2;
/// ClientNetworkAnimator's synchronization data, the same on every animal (captured).
const ANIMATOR_SYNC: &str = "0d0000002101010901110119050000803f010000000014022eda01000000000000803f";
/// ClientNetworkTransform state bits: position x/y/z, rotation y, teleporting, interpolate,
/// synchronization, reliable.
const TRANSFORM_BITS: u32 = 2 | 4 | 8 | 32 | 1024 | 2048 | 32768 | 524288;
/// AIAnimalStateController.State.
const IDLE: u8 = 1;
const WALK: u8 = 2;
/// PetProxy._petDelay.
const PET_DELAY: f32 = 30.0;
/// AnimalCustomizationController variants and pattern masks: every animal takes index 5 (the
/// highest any trait or fallback gives), so the clamp never bites.
const VARIANTS: i32 = 6;
const PATTERNS: i32 = 6;
/// DataConfig.GeneticTraitType.
const SPECIES: i32 = 1;
const COLOR_A: i32 = 2;
const COLOR_B: i32 = 3;
const PATTERN_COLOR: i32 = 4;
const PATTERN: i32 = 5;
const VARIANT: i32 = 6;
const BIOLUMINESCENCE: i32 = 7;
const SIZE: i32 = 8;
/// Enum.GetNames(typeof(GeneticTraitType)).Length: a sequence's inventory size.
const TRAIT_TYPES: i32 = 9;

/// InventorySpawnContent's serialized fields.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Params {
    distance: f32,
    hunger_rate: f32,
    hunger_eat: f32,
    feeder: Option<usize>,
    feeder_range: f32,
    growth_rate: f32,
    growth_min: f32,
}

impl Params {
    /// The class defaults (the wild spawner prefab: the hunger rate it sends is 0.1).
    fn wild() -> Params {
        Params { distance: 200.0, hunger_rate: 0.1, hunger_eat: 25.0, feeder: None, feeder_range: 50.0, growth_rate: 0.01, growth_min: 30.0 }
    }

    fn of(game: &Game, c: &Value) -> Params {
        let f = |k: &str, d: f32| c[k].as_f64().map(|v| v as f32).unwrap_or(d);
        let d = Params::wild();
        Params {
            distance: f("_distanceToDisable", d.distance),
            hunger_rate: f("_hungerDecreasePerSecond", d.hunger_rate),
            hunger_eat: f("_hungerThresholdToEat", d.hunger_eat),
            feeder: c["_feederGroup"].as_str().and_then(|r| game.data.group_of_ref(r)),
            feeder_range: f("_feederRange", d.feeder_range),
            growth_rate: f("_growthIncreasePerSecond", d.growth_rate),
            growth_min: f("_growthMinPercentValue", d.growth_min),
        }
    }
}

/// One InventorySpawnContent: an animal shelter, or a wild spawner instance.
#[derive(serde::Serialize, serde::Deserialize)]
struct Spawner {
    position: [f32; 3],
    /// Its planet: on another one the shelter's object is inactive (no animals).
    planet: i32,
    inventory: i32,
    params: Params,
    /// _spawned: DNA sequence -> its animal (None: known, not spawned).
    spawned: IndexMap<i32, Option<u64>>,
    feeders: IndexSet<i32>,
    last_update: f32,
    /// The world object whose unit multiplier the sequences feed (WorldUnitMultiplierViaInventory).
    multiplier: Option<i32>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Animal {
    sequence: i32,
    /// GrowthProxy._currentGrowth on the server.
    growth: f32,
}

/// SpawnAnimalsDelayed waiting for its next frame.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
    wo: i32,
    position: [f32; 3],
    range: [i32; 2],
    traits: Vec<Value>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// InventorySpawnContent instances, by world object (shelter, or the wild spawner's id).
    spawners: IndexMap<i32, Spawner>,
    by_inventory: HashMap<i32, i32>,
    animals: HashMap<u64, Animal>,
    /// WorldAnimalSpawnerHandler._spawnersCount.
    counts: HashMap<i32, i32>,
    pending: Vec<Pending>,
}

/// The seed SpawnAnimalsDelayed derives from the world seed and the spawner's position: Mono
/// does the float arithmetic in double precision (matched against the real server's spawns).
pub fn spawner_seed(world_seed: u32, p: [f32; 3]) -> u32 {
    let f = |v: f32| v as f64;
    (f(world_seed as f32) / f((p[1] + 1.0).abs()) + f(p[0].abs()) - f(p[2].abs())) as u32
}

/// float -> half (IEEE 754 binary16), round to nearest.
fn half(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = b & 0x7f_ffff;
    if v == 0.0 || exp <= 0 {
        return sign;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    let h = ((exp as u32) << 10) | (mantissa >> 13);
    let round = (mantissa >> 12) & 1;
    sign | (h + round) as u16
}

/// Color.HSVToRGB(Random hue, saturation, value), alpha 1: UnityEngine.Random.ColorHSV().
fn random_color(game: &mut Game) -> [f32; 4] {
    let (h, s, v) = (game.rng.rangef(0.0, 1.0), game.rng.rangef(0.0, 1.0), game.rng.rangef(0.0, 1.0));
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    let (r, g, b) = match i as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b, 1.0]
}

fn component<'a>(game: &'a Game, group: usize, ty: &str) -> Option<&'a Value> {
    game.data.prefab_components(&game.data.groups[group].id, ty).into_iter().next()
}

/// AnimalList: (species id, prefab name).
fn animal_list(game: &Game) -> Vec<(i32, String)> {
    let list = game.data.asset("AnimalList:AnimalList").and_then(|a| a["animals"].as_array()).cloned().unwrap_or_default();
    list.iter().map(|a| (a["id"].as_i64().unwrap_or(0) as i32, a["prefab"].as_str().unwrap_or("").trim_start_matches("GameObject:").to_string())).collect()
}

// ---- WorldAnimalSpawnerHandler ------------------------------------------------------------

pub fn rpc(game: &mut Game, client: u64, object: u64, name: &str, a: &Args) -> bool {
    match name {
        "WorldAnimalSpawnerHandler.SpawnerRegisteredServerRpc" => {
            let wo = a.i(0);
            let s = &mut game.ext.animals.spawn;
            if !s.counts.contains_key(&wo) {
                s.counts.insert(wo, 0);
                game.net.rpc_scene(Target::Only(client), "WorldAnimalSpawnerHandler.SendSpawnerDataClientRpc", &[json!(wo)]);
            }
            *game.ext.animals.spawn.counts.get_mut(&wo).unwrap() += 1;
        }
        "WorldAnimalSpawnerHandler.SpawnerUnregisteredServerRpc" => spawner_unregistered(game, a.i(0)),
        "WorldAnimalSpawnerHandler.SpawnAnimalsServerRpc" => {
            // Wild spawners ask for a few animals; a client asking for millions would make
            // the server create them all in one tick.
            let range = a.0.get(2).map(|v| [0, 1].map(|i| v[i].as_i64().unwrap_or(0).clamp(0, 20) as i32)).unwrap_or([0, 0]);
            let traits = a.0.get(3).and_then(Value::as_array).cloned().unwrap_or_default();
            game.ext.animals.spawn.pending.push(Pending { wo: a.i(0), position: a.v3(1), range, traits });
        }
        "PetProxy.ActionPetServerRpc" if game.ext.animals.spawn.animals.contains_key(&object) => pet(game, client, object),
        "GrowthProxy.GetCurrentGrowthServerRpc" if game.ext.animals.spawn.animals.contains_key(&object) => {
            let growth = game.ext.animals.spawn.animals[&object].growth;
            game.net.rpc(Target::Only(client), object, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(growth)]);
        }
        "GrowthProxy.ResetCurrentGrowthServerRpc" if game.ext.animals.spawn.animals.contains_key(&object) => reset_growth(game, object, a.f(0)),
        _ => return false,
    }
    true
}

fn spawner_unregistered(game: &mut Game, wo: i32) {
    let s = &mut game.ext.animals.spawn;
    // The game's dictionary throws on an id it never counted.
    let Some(count) = s.counts.get_mut(&wo) else { return };
    *count -= 1;
    if *count != 0 {
        return;
    }
    s.counts.remove(&wo);
    if !s.spawners.contains_key(&wo) {
        return;
    }
    let Some(inventory) = game.wos.get(&wo).map(|w| w.linked_inventory) else { return };
    // DestroyAllItemsInside(destroyWo: false), then each removal propagated.
    let items: Vec<i32> = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
    for item in items.into_iter().rev() {
        let keep = game.wos.get(&item).is_some_and(|w| !game.data.groups[w.group].constructible && game.data.groups[w.group].bool("cantBeDestroyed"));
        if !keep {
            game.inventory_remove(inventory, item);
            game.propagate(inventory, item, false);
        }
    }
    remove_spawner(game, wo);
}

/// SpawnAnimalsDelayed after its first frame: the spawner instance starts (its world object and
/// inventory), then DNA sequences seeded from the world seed and position go into it.
fn spawn_delayed(game: &mut Game, p: Pending) {
    // Scene world objects have positive ids; 0 would make a fresh one.
    if p.wo <= 0 {
        return;
    }
    if !game.ext.animals.spawn.spawners.contains_key(&p.wo) {
        if !game.wos.contains_key(&p.wo) {
            // WorldObjectFromScene on the spawner prefab (its group is not in the game data).
            let Some(group) = game.data.group("DNASequence") else { return };
            game.create_world_object(group, p.wo, false);
            let planet = game.planet_hash();
            let w = game.wos.get_mut(&p.wo).unwrap();
            w.position = p.position;
            w.rotation = [0.0, 0.0, 0.0, 1.0];
            w.planet = planet;
        }
        add_spawner(game, p.wo, p.position, Params::wild(), None);
    }
    let Some(inventory) = game.wos.get(&p.wo).map(|w| w.linked_inventory).filter(|i| game.inventories.contains_key(i)) else { return };
    let world_seed = game.save.settings["worldSeed"].as_u64().unwrap_or(0) as u32;
    let mut random = MathRandom::new(spawner_seed(world_seed, p.position));
    // Traits by type, in first-seen order.
    let mut by_type: IndexMap<i32, Vec<&Value>> = IndexMap::new();
    for t in &p.traits {
        by_type.entry(t[0].as_i64().unwrap_or(0) as i32).or_default().push(t);
    }
    let (Some(dna), Some(trait_group)) = (game.data.group("DNASequence"), game.data.group(crate::genetics::TRAIT_GROUP)) else { return };
    // Unity's NextInt needs min < max; a reversed range would wrap to billions.
    let (min, max) = (p.range[0].min(p.range[1]), p.range[0].max(p.range[1]));
    let count = random.next_int(min, max + 1).clamp(0, 20);
    for _ in 0..count {
        let sequence = game.create_world_object(dna, 0, false);
        let traits = game.create_inventory(TRAIT_TYPES, -1, sequence, &[], &[]);
        for list in by_type.values() {
            let chosen = list[random.next_int(0, list.len() as i32) as usize].clone();
            let t = game.create_world_object(trait_group, 0, false);
            crate::genetics::set_trait_serialized(game.wos.get_mut(&t).unwrap(), &chosen);
            game.inventory_add(traits, t, true);
        }
        // ponytail: without a Species trait the game adds an unregistered Species-0 trait (no
        // such animal, nothing spawns); every spawner in the game data has one.
        game.wos.get_mut(&sequence).unwrap().growth = 100.0;
        game.inventory_add(inventory, sequence, true);
        game.propagate(inventory, sequence, true);
    }
}

// ---- InventorySpawnContent ----------------------------------------------------------------

pub fn on_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let (group, position) = (w.group, w.position);
    if let Some(c) = component(game, group, "InventorySpawnContent") {
        let params = Params::of(game, c);
        let multiplier = component(game, group, "WorldUnitMultiplierViaInventory").is_some_and(|m| m["influencedByHunger"].as_bool() == Some(true)).then_some(wo);
        add_spawner(game, wo, position, params, multiplier);
    }
    // OnConstructibleInstantiated: a feeder in range of a shelter.
    let s = &mut game.ext.animals.spawn;
    for sp in s.spawners.values_mut() {
        if sp.params.feeder == Some(group) && distance2(position, sp.position).sqrt() < sp.params.feeder_range {
            sp.feeders.insert(wo);
        }
    }
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    if game.ext.animals.spawn.spawners.contains_key(&wo.id) {
        remove_spawner(game, wo.id);
    }
    for sp in game.ext.animals.spawn.spawners.values_mut() {
        sp.feeders.shift_remove(&wo.id);
    }
}

/// StartImpl + RegisterToInventory: the instance watches its inventory; what is inside is known
/// but spawns only when a player comes near (the local-player-started callback).
fn add_spawner(game: &mut Game, wo: i32, position: [f32; 3], params: Params, multiplier: Option<i32>) {
    let Some((mut inventory, planet)) = game.wos.get(&wo).map(|w| (w.linked_inventory, w.planet_hash())) else { return };
    if !game.inventories.contains_key(&inventory) {
        inventory = game.create_inventory(50, -1, wo, &[], &[]);
    }
    let spawned = game.inventories.get(&inventory).map(|i| i.items.iter().map(|id| (*id, None)).collect()).unwrap_or_default();
    // InitFeedersInRange.
    let feeders = match params.feeder {
        Some(g) => game.wos.values().filter(|f| f.group == g && f.net.is_some() && distance2(f.position, position).sqrt() < params.feeder_range).map(|f| f.id).collect(),
        None => IndexSet::new(),
    };
    *game.watchers.entry(inventory).or_default().entry(0).or_insert(0) += 1;
    let s = &mut game.ext.animals.spawn;
    s.by_inventory.insert(inventory, wo);
    s.spawners.insert(wo, Spawner { position, planet, inventory, params, spawned, feeders, last_update: 0.0, multiplier });
}

/// The instance goes away (OnDisable: DespawnAll; OnDestroy: stop watching).
fn remove_spawner(game: &mut Game, wo: i32) {
    despawn_all(game, wo);
    let Some(sp) = game.ext.animals.spawn.spawners.shift_remove(&wo) else { return };
    game.ext.animals.spawn.by_inventory.remove(&sp.inventory);
    game.stop_inventory_watch(sp.inventory, 0);
}

/// OnInventoryModified.
pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    let Some(key) = game.ext.animals.spawn.by_inventory.get(&inventory).copied() else { return };
    let sp = &game.ext.animals.spawn.spawners[&key];
    if added {
        if sp.params.feeder.is_some() {
            if let Some(w) = game.wos.get_mut(&wo).filter(|w| w.hunger == 0.0) {
                w.hunger = 100.0;
            }
        }
        let sp = game.ext.animals.spawn.spawners.get_mut(&key).unwrap();
        if matches!(sp.spawned.first(), Some((_, None))) {
            sp.spawned.insert(wo, None);
        } else {
            spawn_animal(game, key, wo, 0, false);
        }
    } else if let Some(animal) = game.ext.animals.spawn.spawners.get_mut(&key).unwrap().spawned.shift_remove(&wo) {
        if let Some(net) = animal {
            despawn(game, net);
        }
    }
}

fn despawn(game: &mut Game, net: u64) {
    game.ext.animals.spawn.animals.remove(&net);
    game.net.despawn(net);
}

fn despawn_all(game: &mut Game, key: i32) {
    let Some(sp) = game.ext.animals.spawn.spawners.get_mut(&key) else { return };
    let nets: Vec<u64> = sp.spawned.values_mut().filter_map(Option::take).collect();
    for net in nets {
        despawn(game, net);
    }
}

/// A trait of a sequence: (value, color); GetTraitFromSequence's random stand-in when missing.
fn sequence_trait(game: &mut Game, sequence: i32, ty: i32) -> (i32, [f32; 4]) {
    let inventory = game.wos.get(&sequence).map(|w| w.linked_inventory).and_then(|i| game.inventories.get(&i));
    if let Some(t) = inventory.and_then(|i| i.items.iter().filter_map(|id| game.wos.get(id)).find(|w| w.trait_type == ty)) {
        return (t.trait_value, t.color);
    }
    let empty = inventory.is_none_or(|i| i.items.is_empty());
    let mut color = random_color(game);
    let value = match ty {
        BIOLUMINESCENCE if empty => game.rng.range(0, 11),
        SIZE | VARIANT | PATTERN if empty => game.rng.range(0, 6),
        BIOLUMINESCENCE | SIZE | VARIANT | PATTERN => 0,
        PATTERN_COLOR => {
            if !empty {
                color = [1.0; 4];
            }
            0
        }
        _ => {
            let v = game.rng.range(0, 6);
            if ty == SPECIES {
                let list = animal_list(game);
                list.get(game.rng.range(0, list.len() as i32) as usize).map(|a| a.0).unwrap_or(v)
            } else {
                v
            }
        }
    };
    (value, color)
}

fn animal_layout(hash: u32, tick: i32) -> Layout {
    let var = |name: &str, ty: &str, bytes: Vec<u8>| VarDef { name: name.into(), ty: ty.into(), bytes: Some(bytes.iter().map(|b| format!("{b:02x}")).collect()) };
    let behaviour = |class: &str, id: u16, vars: Vec<VarDef>| BehaviourDef { class: class.into(), behaviour_id: id, vars };
    let behaviours = vec![
        behaviour(
            "AIAnimalStateController",
            AI,
            vec![
                var("_randomState", "NetworkVariable<UInt32>", Writer::new().packed_u32(0).0.clone()),
                var("_spawnPosition", "NetworkVariable<Vector3>", vec![0; 12]),
                // A byte-sized enum (State : byte).
                var("_currentState", "NetworkVariable<Byte>", vec![0]),
                var("_avoidancePriority", "NetworkVariable<Int32>", enc::int(0)),
                var("_disabled", "NetworkVariable<Boolean>", enc::bool(false)),
                var("_hungerDecreaseSpeed", "NetworkVariable<Single>", enc::float(0.0)),
            ],
        ),
        behaviour("ClientNetworkTransform", TRANSFORM, vec![]),
        behaviour("ClientNetworkAnimator", ANIMATOR, vec![]),
        behaviour("AnimalCustomizationController", 3, vec![var("_currentData", "NetworkVariable<CustomizationData>", vec![0; 60])]),
        behaviour("WorldObjectAssociatedProxy", 4, vec![]),
        behaviour("GrowthProxy", 5, vec![]),
        behaviour("PetProxy", 6, vec![]),
    ];
    // Custom sync data: the transform's state (position written at serialization), the animator's.
    let mut state = Writer::new();
    state.packed_u32(TRANSFORM_BITS).packed_i32(tick);
    let at = state.len();
    state.vec3([0.0; 3]).f32(0.0);
    let mut tail = Writer::new();
    tail.u8(2).u16(TRANSFORM).i32(state.len() as i32);
    let tail_position = tail.len() + at;
    tail.bytes(&state.0);
    let animator = crate::world::hex(ANIMATOR_SYNC);
    tail.u16(ANIMATOR).i32(animator.len() as i32).bytes(&animator);
    Layout { hash, flags: FLAGS, scale: [1.0; 3], behaviours, tail: tail.0, tail_position: Some(tail_position) }
}

/// SpawnWorldObject: the sequence's animal, owned by `owner` (0: the first animal's owner, or
/// the server), with its hunger speed and looks.
fn spawn_animal(game: &mut Game, key: i32, sequence: i32, owner: u64, random_position: bool) {
    let species = sequence_trait(game, sequence, SPECIES).0;
    let Some(prefab) = animal_list(game).into_iter().find(|a| a.0 == species).map(|a| a.1) else { return };
    let Some(hash) = PREFABS.iter().find(|p| p.0 == prefab).map(|p| p.1) else { return };
    let sp = &game.ext.animals.spawn.spawners[&key];
    let (position, params) = (sp.position, sp.params.clone());
    let first_owner = match sp.spawned.first() {
        Some((_, Some(net))) => game.net.objects.get(net).map(|o| o.owner),
        _ => None,
    };
    let growth = {
        let w = game.wos.get_mut(&sequence).unwrap();
        w.growth = w.growth.max(params.growth_min);
        w.growth
    };
    let owner = if owner != 0 { owner } else { first_owner.unwrap_or(0) };
    let mut o = NetObj::from_layout(&animal_layout(hash, game.tick_number()));
    o.position = position;
    o.wo = Some(sequence);
    o.owner = owner;
    if owner == 0 {
        // OnNetworkSpawn as the owner: the server picks the wandering state.
        let avoidance = game.rng.range(0, 100);
        let mut random = MathRandom::new(game.rng.range(1, 999999) as u32);
        let state = if random.next_bool() { IDLE } else { WALK };
        o.init("AIAnimalStateController", "_avoidancePriority", enc::int(avoidance));
        o.init("AIAnimalStateController", "_spawnPosition", position.iter().flat_map(|v| v.to_le_bytes()).collect());
        o.init("AIAnimalStateController", "_currentState", vec![state]);
        o.init("AIAnimalStateController", "_randomState", Writer::new().packed_u32(random.state).0.clone());
    }
    let net = game.net.spawn(o);
    game.net.set(net, "AIAnimalStateController", "_hungerDecreaseSpeed", enc::float(params.hunger_rate));
    if random_position && params.feeder.is_none() {
        game.net.rpc(Target::Only(owner), net, "AIAnimalStateController.MoveToRandomPositionClientRpc", &[]);
    }
    let variant = sequence_trait(game, sequence, VARIANT).0.min(VARIANTS - 1);
    let belly = sequence_trait(game, sequence, COLOR_A).1;
    let top = sequence_trait(game, sequence, COLOR_B).1;
    let pattern_color = sequence_trait(game, sequence, PATTERN_COLOR).1;
    let pattern = sequence_trait(game, sequence, PATTERN).0.min(PATTERNS - 1);
    let scale = sequence_trait(game, sequence, SIZE).0 + 1;
    let glow = sequence_trait(game, sequence, BIOLUMINESCENCE).0;
    let mut data = Writer::new();
    data.bytes(&(variant as i16).to_le_bytes()).u16(0).bytes(&enc::color(belly)).bytes(&enc::color(top)).bytes(&enc::color(pattern_color));
    data.bytes(&(pattern as i16).to_le_bytes()).bytes(&(scale as i16).to_le_bytes()).u16(half(glow as f32)).u16(0);
    game.net.set(net, "AnimalCustomizationController", "_currentData", data.0);
    game.ext.animals.spawn.spawners.get_mut(&key).unwrap().spawned.insert(sequence, Some(net));
    game.ext.animals.spawn.animals.insert(net, Animal { sequence, growth });
}

/// Clients move their animals; the sequence they stand for stays in its inventory, unplaced.
pub fn keep_sequences_unplaced(game: &mut Game) {
    for a in game.ext.animals.spawn.animals.values() {
        if let Some(w) = game.wos.get_mut(&a.sequence) {
            w.position = [0.0; 3];
        }
    }
}

/// One frame: animals' growth (AIAnimalStateController.UpdateGrowth), every instance's
/// UpdateComponent, then the spawn coroutines due this frame.
pub fn frame(game: &mut Game) {
    let animals: Vec<u64> = game.ext.animals.spawn.animals.keys().copied().collect();
    for net in animals {
        let a = &game.ext.animals.spawn.animals[&net];
        let Some(growth) = game.wos.get(&a.sequence).map(|w| w.growth) else { continue };
        if (growth - a.growth).abs() > 0.1 {
            reset_growth(game, net, growth);
        }
    }
    let keys: Vec<i32> = game.ext.animals.spawn.spawners.keys().copied().collect();
    for key in keys {
        update_component(game, key);
    }
    for p in std::mem::take(&mut game.ext.animals.spawn.pending) {
        spawn_delayed(game, p);
    }
}

/// GrowthProxy.SetGrowth(forceUpdate) / ResetCurrentGrowthServerRpc: everyone gets the growth.
fn reset_growth(game: &mut Game, net: u64, value: f32) {
    let Some(a) = game.ext.animals.spawn.animals.get_mut(&net) else { return };
    a.growth = value;
    if let Some(w) = game.wos.get_mut(&a.sequence) {
        w.growth = value;
    }
    game.net.rpc(Target::All, net, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(value)]);
}

/// WorldUnitMultiplierViaInventory.RefreshGenerationMultiplier (influenced by hunger).
fn refresh_multiplier(game: &mut Game, holder: i32, sequence: i32, multiplier: f32) {
    let Some(unit) = crate::model::unit_index("Animals") else { return };
    let Some(seq) = game.wos.get_mut(&sequence) else { return };
    let removed = -seq.unit_multiplier[unit];
    seq.unit_multiplier[unit] = 0.0;
    let mut added = game.data.groups[seq.group].units[unit] / 100.0 * multiplier;
    if seq.hunger < 0.0 {
        added *= 0.5;
    }
    seq.unit_multiplier[unit] = added;
    let Some(h) = game.wos.get_mut(&holder) else { return };
    h.unit_multiplier[unit] += removed + added;
    let planet = h.planet_hash();
    game.units.force_reset(planet);
}

fn update_component(game: &mut Game, key: i32) {
    let now = game.time;
    let sp = &game.ext.animals.spawn.spawners[&key];
    if sp.spawned.is_empty() {
        return;
    }
    if sp.params.feeder.is_some() && sp.last_update > 0.0 {
        feed(game, key, now - sp.last_update);
    }
    let sp = game.ext.animals.spawn.spawners.get_mut(&key).unwrap();
    sp.last_update = now;
    if sp.planet != game.planet_hash() {
        // Not active in the hierarchy: OnDisable despawned everything.
        despawn_all(game, key);
        return;
    }
    if super::players(game).is_empty() {
        return;
    }
    // ponytail: NetworkShow/NetworkHide per player is not ported (no per-client visibility in
    // net.rs): every client sees every animal.
    let sp = &game.ext.animals.spawn.spawners[&key];
    let (position, range2) = (sp.position, sp.params.distance * sp.params.distance);
    let first = sp.spawned.first().map(|(_, n)| *n);
    let waiting: Vec<i32> = sp.spawned.iter().filter(|(_, n)| n.is_none()).map(|(s, _)| *s).collect();
    let nets: Vec<u64> = sp.spawned.values().flatten().copied().collect();
    let (closest, d2) = nearest(game, position);
    match first {
        Some(None) if d2 < range2 => {
            for sequence in waiting {
                if game.wos.contains_key(&sequence) {
                    spawn_animal(game, key, sequence, closest, true);
                }
            }
        }
        Some(Some(first)) => {
            if d2 > range2 * 1.5 {
                despawn_all(game, key);
                return;
            }
            let owner = game.net.objects.get(&first).map(|o| o.owner).unwrap_or(0);
            if owner == closest || player_distance2(game, owner, position) <= range2 * 0.5 {
                return;
            }
            for net in nets {
                game.net.change_owner(net, closest);
            }
        }
        _ => {}
    }
}

/// Hunger and growth of a shelter's animals over `dt` seconds; hungry ones eat from feeders.
fn feed(game: &mut Game, key: i32, dt: f32) {
    let sp = &game.ext.animals.spawn.spawners[&key];
    let (params, multiplier) = (sp.params.clone(), sp.multiplier);
    let sequences: Vec<i32> = sp.spawned.keys().copied().collect();
    for sequence in sequences {
        let Some(w) = game.wos.get_mut(&sequence) else { continue };
        let fed = w.hunger > 0.0;
        w.hunger = (w.hunger - dt * params.hunger_rate).max(-100.0);
        if fed && w.hunger < 0.0 {
            if let Some(m) = multiplier {
                refresh_multiplier(game, m, sequence, 1.0);
            }
        }
        if game.wos[&sequence].hunger < params.hunger_eat {
            let feeders: Vec<i32> = game.ext.animals.spawn.spawners[&key].feeders.iter().copied().collect();
            'feeders: for feeder in feeders {
                let Some(inventory) = game.wos.get(&feeder).map(|f| f.linked_inventory).filter(|i| game.inventories.contains_key(i)) else { continue };
                for item in game.inventories[&inventory].items.clone() {
                    let Some(g) = game.wos.get(&item).map(|w| &game.data.groups[w.group]) else { continue };
                    if g.constructible || !matches!(g.usable_type(), "Drinkable" | "Breathable" | "Eatable" | "Purify" | "AquireTokens") {
                        continue;
                    }
                    let (value, unit) = (g.fields["value"].as_i64().unwrap_or(0) as f32, g.unit("Animals"));
                    let w = game.wos.get_mut(&sequence).unwrap();
                    w.hunger = (w.hunger.max(0.0) + value).min(100.0);
                    if let Some(m) = multiplier {
                        refresh_multiplier(game, m, sequence, (unit / 100.0).max(1.0));
                    }
                    game.inventories_rpc(0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(item), json!(inventory), json!(true)]));
                    break 'feeders;
                }
            }
        }
        let w = game.wos.get_mut(&sequence).unwrap();
        if w.hunger >= 0.0 {
            w.growth = (w.growth + dt * params.growth_rate).min(100.0);
        }
    }
}

// ---- PetProxy -----------------------------------------------------------------------------

/// ActionPetServerRpc: at most once per delay and not when starving, petting gives a genetic
/// trait: mostly a copy of one of the animal's own, else a random lootable one.
fn pet(game: &mut Game, client: u64, object: u64) {
    let sequence = game.ext.animals.spawn.animals[&object].sequence;
    let time = game.time;
    let Some(w) = game.wos.get_mut(&sequence) else { return };
    if (w.pet_time != 0.0 && time - w.pet_time <= PET_DELAY) || w.hunger < 0.0 {
        return;
    }
    w.pet_time = time;
    let inventory = w.linked_inventory;
    let Some(items) = game.inventories.get(&inventory).map(|i| i.items.clone()).filter(|i| !i.is_empty()) else { return };
    let copy = |game: &mut Game| {
        let i = game.rng.range(0, items.len() as i32) as usize;
        let source = game.wos.get(&items[i])?.clone();
        let id = game.create_world_object(source.group, 0, true);
        let w = game.wos.get_mut(&id).unwrap();
        (w.trait_type, w.color, w.trait_value) = (source.trait_type, source.color, source.trait_value);
        Some(id)
    };
    let created = if game.rng.range(0, 100) <= 70 {
        copy(game)
    } else {
        match random_lootable_trait(game) {
            None => copy(game),
            Some(t) => game.data.group(crate::genetics::TRAIT_GROUP).map(|group| {
                let id = game.create_world_object(group, 0, true);
                crate::genetics::set_trait(game.wos.get_mut(&id).unwrap(), &t);
                id
            }),
        }
    };
    let Some(created) = created else { return };
    let group = game.wos[&created].group;
    if game.data.groups[group].id != crate::genetics::TRAIT_GROUP {
        game.destroy_world_object(created, false);
        return;
    }
    let Some(backpack) = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.inventory) else { return };
    let full = game.inventories.get(&backpack).is_none_or(|i| i.is_full());
    if full {
        let p = game.net.objects.get(&object).map(|o| o.position).unwrap_or_default();
        game.drop_on_floor(created, [p[0], p[1] + 2.0, p[2]], [0.0, 0.0, 0.0, 1.0], 0.0, true, true, 0, 0.0, false, 0);
    } else {
        game.add_existing_item(created, backpack, true, 0);
    }
    let hash = game.data.groups[group].hash;
    game.net.rpc(Target::Only(client), object, "PetProxy.ActionPetClientRpc", &[json!(full), json!(hash)]);
}

/// GeneticTraitHandler.GetRandomGeneticTraitWithRarity.
fn random_lootable_trait(game: &mut Game) -> Option<Value> {
    let lootable: Vec<Value> = game.data.assets.iter().filter(|(k, v)| k.starts_with("GeneticTraitData:") && v["canBeLooted"].as_bool() == Some(true)).map(|(_, v)| v.clone()).collect();
    if lootable.is_empty() {
        return None;
    }
    for _ in 0..1000 {
        let t = &lootable[game.rng.range(0, lootable.len() as i32) as usize];
        if game.rng.range(0, 100) as i64 >= t["lootChance"].as_i64().unwrap_or(0) {
            return Some(t.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(mut s: u32) -> u32 {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    }

    #[test]
    fn math_random_matches_the_animals_random_state() {
        // Captured spawns: _randomState after new Random(Range(1, 999999)) and one NextBool,
        // with the state that NextBool picked (Idle 1, Walk 2).
        for (state, current) in [(0xda82fa37u32, WALK), (0x753c648a, IDLE), (0x48e44369, IDLE)] {
            let seed = (1..999_999u32).find(|s| xorshift(xorshift(*s)) == state).expect("a seed in range");
            let mut r = MathRandom::new(seed);
            assert_eq!(if r.next_bool() { IDLE } else { WALK }, current);
            assert_eq!(r.state, state);
        }
    }

    /// The Aqualis spawner's trait list as (type, value or color index) for the spawn test.
    fn aqualis() -> Vec<Value> {
        let c = |r: f32, g: f32, b: f32| json!([r, g, b, 0.0]);
        let mut t = vec![json!([SPECIES, [0.0, 0.0, 0.0, 0.0], 7])];
        let pattern_colors = [c(0.415, 0.415, 0.415), c(0.404, 0.604, 0.267), c(0.349, 0.502, 0.624), c(0.834, 0.887, 0.322), c(0.220, 0.169, 0.651)];
        t.extend(pattern_colors.iter().map(|c| json!([PATTERN_COLOR, c, 0])));
        t.push(json!([SIZE, [0.0, 0.0, 0.0, 0.0], 1]));
        t.extend((1..4).map(|v| json!([PATTERN, [0.0, 0.0, 0.0, 0.0], v])));
        t.extend((1..4).map(|v| json!([VARIANT, [0.0, 0.0, 0.0, 0.0], v])));
        t.extend([c(0.914, 0.592, 0.486), c(1.0, 0.988, 0.722), c(0.953, 0.953, 0.953)].iter().map(|c| json!([COLOR_A, c, 0])));
        t.extend([c(0.914, 0.592, 0.486), c(0.588, 0.980, 0.933), c(0.788, 1.0, 0.647), c(0.557, 0.773, 0.945)].iter().map(|c| json!([COLOR_B, c, 0])));
        t.extend([c(0.349, 0.502, 0.624), c(0.834, 0.887, 0.322), c(0.220, 0.169, 0.651), c(1.0, 0.948, 0.948)].iter().map(|c| json!([PATTERN_COLOR, c, 0])));
        t
    }

    /// What SpawnAnimalsDelayed picks: per animal, (variant, pattern, ColorA r, ColorB r, pattern color r).
    fn picks(seed: u32, range: [i32; 2]) -> Vec<(i64, i64, f64, f64, f64)> {
        let traits = aqualis();
        let mut by_type: IndexMap<i64, Vec<&Value>> = IndexMap::new();
        for t in &traits {
            by_type.entry(t[0].as_i64().unwrap()).or_default().push(t);
        }
        let mut random = MathRandom::new(seed);
        let n = random.next_int(range[0], range[1] + 1);
        (0..n)
            .map(|_| {
                let chosen: HashMap<i64, &Value> = by_type.iter().map(|(ty, list)| (*ty, list[random.next_int(0, list.len() as i32) as usize])).collect();
                let r = |ty: i32| (chosen[&(ty as i64)][1][0].as_f64().unwrap() * 1000.0).round() / 1000.0;
                (chosen[&(VARIANT as i64)][2].as_i64().unwrap(), chosen[&(PATTERN as i64)][2].as_i64().unwrap(), r(COLOR_A), r(COLOR_B), r(PATTERN_COLOR))
            })
            .collect()
    }

    #[test]
    fn wild_spawns_are_seeded_like_the_game() {
        // The real server, world seed 620174666, Aqualis traits.
        let seed = spawner_seed(620174666, [30.5, 3.25, 20.75]);
        assert_eq!(picks(seed, [3, 5]), vec![(1, 1, 0.953, 0.557, 0.349), (2, 2, 0.914, 0.557, 1.0), (3, 3, 0.953, 0.557, 0.834)]);
        let seed = spawner_seed(620174666, [-20.0, 7.0, 35.0]);
        assert_eq!(
            picks(seed, [2, 6]),
            vec![(1, 1, 0.914, 0.557, 0.349), (2, 1, 0.914, 0.914, 0.220), (2, 2, 0.914, 0.914, 0.220), (3, 1, 0.914, 0.588, 0.415), (1, 1, 0.953, 0.914, 0.349)]
        );
    }

    /// A placed building of a group, instantiated like a construction.
    fn place(game: &mut Game, group: &str, position: [f32; 3]) -> i32 {
        let g = game.data.group(group).unwrap();
        let id = game.create_world_object(g, 0, true);
        let planet = game.planet_hash();
        let w = game.wos.get_mut(&id).unwrap();
        (w.position, w.rotation, w.planet) = (position, [0.0, 0.0, 0.0, 1.0], planet);
        game.instantiate(id, false, 0);
        id
    }

    #[test]
    fn shelter_animals_get_hungry_eat_from_feeders_and_grow() {
        let Some(mut game) = crate::meteo::tests::game() else { return };
        let shelter = place(&mut game, "AnimalShelter1", [100.0, 0.0, 100.0]);
        let feeder = place(&mut game, "AnimalFeeder1", [110.0, 0.0, 100.0]);
        let food = game.create_world_object(game.data.group("AnimalFood1").unwrap(), 0, true);
        game.inventory_add(game.wos[&feeder].linked_inventory, food, true);
        // A Gazelle sequence (species 3) goes into the shelter.
        let sequence = game.create_world_object(game.data.group("DNASequence").unwrap(), 0, true);
        let traits = game.create_inventory(TRAIT_TYPES, 0, sequence, &[], &[]);
        let species = game.create_world_object(game.data.group(crate::genetics::TRAIT_GROUP).unwrap(), 0, true);
        crate::genetics::set_trait_serialized(game.wos.get_mut(&species).unwrap(), &json!([SPECIES, [0, 0, 0, 0], 3]));
        game.inventory_add(traits, species, true);
        assert!(game.add_existing_item(sequence, game.wos[&shelter].linked_inventory, true, 0));

        let (&net, _) = game.ext.animals.spawn.animals.iter().next().expect("the animal spawned");
        let o = &game.net.objects[&net];
        assert_eq!((o.hash, o.owner, o.position), (0x460320a1, 0, [100.0, 0.0, 100.0]));
        assert!(matches!(game.net.get(net, "AIAnimalStateController", "_hungerDecreaseSpeed"), Some(crate::net::VarValue::Plain(b)) if *b == enc::float(0.05)));
        assert_eq!((game.wos[&sequence].hunger, game.wos[&sequence].growth), (100.0, 30.0));

        game.time = 1.0;
        frame(&mut game);
        game.time = 1601.0;
        frame(&mut game);
        // 80 hunger gone, below 25: it ate the food (+40), and grew 16.
        let w = &game.wos[&sequence];
        assert_eq!((w.hunger, w.growth), (60.0, 46.0));
        assert!(!game.wos.contains_key(&food));
        let unit = crate::model::unit_index("Animals").unwrap();
        assert_eq!(game.wos[&sequence].unit_multiplier[unit], 20.0, "DNA x10, food x2");
        // The next frame's AIAnimalStateController.UpdateGrowth tells clients.
        frame(&mut game);
        assert_eq!(game.ext.animals.spawn.animals[&net].growth, 46.0);

        // The sequence taken out: its animal goes.
        let inventory = game.wos[&shelter].linked_inventory;
        game.inventory_remove(inventory, sequence);
        game.propagate(inventory, sequence, false);
        assert!(!game.net.objects.contains_key(&net) && game.ext.animals.spawn.animals.is_empty());
    }

    #[test]
    fn halves_match_the_bioluminescence_bytes() {
        assert_eq!(half(7.0), 0x4700);
        assert_eq!(half(10.0), 0x4900);
        assert_eq!(half(0.0), 0);
        assert_eq!(half(1.0), 0x3c00);
    }

    #[test]
    fn animal_layout_matches_the_captured_spawn_tail() {
        // Sharky as the real server spawned it at (42, 2.5, 40), tick 41963 (captured).
        let l = animal_layout(0x60be7ec3, 41963);
        let mut o = NetObj::from_layout(&l);
        o.position = [42.0, 2.5, 40.0];
        let sync = o.sync();
        let tail = &sync[sync.len() - l.tail.len()..];
        assert_eq!(crate::world::hex("02010016000000736144b33e0a000028420000204000002042000000000200230000000d0000002101010901110119050000803f010000000014022eda01000000000000803f"), tail);
    }
}
