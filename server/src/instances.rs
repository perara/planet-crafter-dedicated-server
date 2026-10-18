//! SpaceCraft.ProceduralInstancesHandler, GenerateOnTrigger and WorldInstanceHandler, server
//! side: procedural wrecks (a generator network object; clients build the wreck from its seed)
//! and the world instances portals open, with the save's procedural instances and the world
//! state's opened instance.
//!
//! What a wreck and an opened world instance hold is generated in generate.rs.
//!
//! What the server cannot do, and what stands in for it:
//! - Wrecks get seeds the extractor laid out (generate::pool_seed).

pub mod generate;

use std::collections::HashSet;

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};

use crate::data::BehaviourDef;
use crate::game::{Args, Game, PlayerState};
use crate::gamedata::Layout;
use crate::model::{floats_string, parse_floats, WorldObject};
use crate::net::{enc, NetObj, Target};
use crate::geometry::MathRandom;
use crate::wire::Writer;

const GENERATOR: &str = "GenerateOnTrigger";
const HANDLER: &str = "WorldInstanceHandler";
/// ProceduralInstancesHandler._generators (GeneratorDifficulty01..03): the prefab's network
/// hash as the real game spawns it, and its difficulty.
const GENERATORS: [(u32, i32); 3] = [(0x398a_5515, 0), (0x42d7_ff3e, 1), (0x953c_390c, 2)];
/// HasTransform | WorldPositionStays | DestroyWithScene.
const GENERATOR_FLAGS: u16 = 0x68;
/// GameConfig.proceduralGeneratorsVersion.
const VERSION: i64 = 16;
/// GenerateOnTrigger.Clear despawns 5 frames after unloading (the real game: 0.17 s).
const DESPAWN_DELAY: f32 = 0.17;
/// The player's CharacterController height, for what drops from a player dying in an instance.
const PLAYER_HEIGHT: f32 = 2.0;

/// One spawned generator (GenerateOnTrigger) of ProceduralInstancesHandler._instances.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Generator {
    pub net: u64,
    /// Index into GENERATORS.
    pub index: usize,
    pub planet: i32,
    pub seed: u32,
    /// _networkObjectGenerated.
    pub generated: bool,
    pub generated_ids: IndexSet<i32>,
    pub dropped_ids: IndexSet<i32>,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    /// Its trigger sphere's radius.
    pub radius: f32,
}

/// WorldInstanceData.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorldInstance {
    pub seed: i32,
    pub time_left: i32,
    pub scene: String,
    pub loot_rarity: i32,
    pub difficulty: i32,
    pub recipe: Vec<i32>,
    pub loaded_from_db: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// Owner (0: the planets' scenes, else the world instance's seed) -> its generators.
    pub instances: IndexMap<i32, Vec<Generator>>,
    /// WorldInstanceHandler._openedWorldInstanceData.
    pub opened: Option<WorldInstance>,
    /// _currentChoices: (seed, time left) per choice.
    pub choices: Vec<(i32, i32)>,
    /// ReduceTimeOpenedWorldInstance: when the time left next goes down.
    next_second: f32,
    /// Generators to despawn: (game time, network object).
    despawns: Vec<(f32, u64)>,
    /// RandomizePropData.ignore of the wreck props (the game keeps it on the prefabs).
    ignored: HashSet<(String, usize)>,
    /// The opened instance scene's PreventBuilding collider.
    prevent_building: Option<generate::Shape>,
}

fn state(game: &mut Game) -> &mut State {
    &mut game.ext.planets.instances
}

// ---- serialization -------------------------------------------------------------------------

/// WorldInstanceDataSerializationExtensions.WriteValueSafe (an int 0 for none).
pub fn encode_instance(data: Option<&WorldInstance>) -> Vec<u8> {
    let mut w = Writer::new();
    let Some(d) = data else {
        w.i32(0);
        return w.0;
    };
    w.i32(d.seed).i32(d.time_left);
    let units: Vec<u16> = d.scene.encode_utf16().collect();
    w.u32(units.len() as u32);
    units.iter().for_each(|u| {
        w.u16(*u);
    });
    w.i32(d.loot_rarity).i32(d.difficulty).i32_array(&d.recipe);
    w.0
}

/// WorldInstanceHandler.InstanceChoices.NetworkSerialize.
pub fn encode_choices(choices: &[(i32, i32)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.i32_array(&choices.iter().map(|c| c.0).collect::<Vec<_>>()).i32_array(&choices.iter().map(|c| c.1).collect::<Vec<_>>());
    w.0
}

fn uint(v: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.packed_u32(v);
    w.0
}

/// WorldInstanceGenerator.GetWorldInstanceDataViaSeed, from the current planet's instance data.
pub fn instance_via_seed(game: &Game, seed: i32, time_left: i32, loaded_from_db: bool) -> Option<WorldInstance> {
    const MAX_LOOT_RARITY: i32 = 5;
    const MAX_DIFFICULTY: i32 = 3;
    const MIN_INGREDIENTS: i32 = 3;
    let planet = game.data.asset(&format!("PlanetData:PlanetData-{}", game.planet_id()))?;
    let data = game.data.asset(planet["instanceScenesData"].as_str()?)?;
    let list = |field: &str| -> Vec<String> { data[field].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect() };
    let (base, rare, scenes) = (list("basePossibleIngredients"), list("rarePossibleIngredients"), list("instanceSceneNames"));
    let mut r = MathRandom::new(seed as u32);
    let loot_rarity = r.next_int(0, MAX_LOOT_RARITY);
    let difficulty = r.next_int(if loot_rarity as f32 > MAX_LOOT_RARITY as f32 * 0.5 { 1 } else { 0 }, MAX_DIFFICULTY);
    let n = r.next_int(MIN_INGREDIENTS, MIN_INGREDIENTS + loot_rarity / 2);
    let mut ingredients = Vec::new();
    for _ in 0..n {
        ingredients.push(base.get(r.next_int(0, base.len() as i32) as usize)?.clone());
    }
    if loot_rarity > 0 {
        for _ in 0..loot_rarity + 1 {
            ingredients.push(rare.get(r.next_int(0, rare.len() as i32) as usize)?.clone());
        }
    }
    let mut ids: Vec<String> = ingredients.iter().filter_map(|i| Some(game.data.asset(i)?["id"].as_str()?.to_string())).collect();
    ids.sort_by(|x, y| crate::inventories::culture_cmp(y, x));
    let scene = scenes.get(r.next_int(0, scenes.len() as i32) as usize)?.clone();
    let recipe = ids.iter().map(|i| crate::data::stable_hash(i)).collect();
    Some(WorldInstance { seed, time_left, scene, loot_rarity, difficulty, recipe, loaded_from_db })
}

// ---- generators ----------------------------------------------------------------------------

fn spawn_generator(game: &mut Game, index: usize, position: [f32; 3], rotation: [f32; 4]) -> u64 {
    let var = |name: &str, ty: &str, bytes: &str| crate::data::VarDef { name: name.into(), ty: ty.into(), bytes: Some(bytes.into()) };
    let layout = Layout {
        hash: GENERATORS[index].0,
        flags: GENERATOR_FLAGS,
        scale: [1.0; 3],
        behaviours: vec![BehaviourDef {
            class: GENERATOR.into(),
            behaviour_id: 0,
            vars: vec![
                var("_seed", "NetworkVariable<UInt32>", "01"),
                var("_owner", "NetworkVariable<Int32>", "09"),
                var("_networkObjectGenerated", "NetworkVariable<Boolean>", "00"),
                var("_planetHash", "NetworkVariable<Int32>", "01"),
            ],
        }],
        tail: Vec::new(),
        tail_position: None,
    };
    let mut o = NetObj::from_layout(&layout);
    o.position = position;
    o.rotation = rotation;
    game.net.spawn(o)
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// GenerateOnTrigger.IsInsideTriggers: only generators of the current planet are active.
fn inside_triggers(game: &Game, g: &Generator, position: [f32; 3]) -> bool {
    g.planet == game.planet_hash() && dist2(g.position, position) <= g.radius * g.radius
}

fn generators(game: &Game) -> impl Iterator<Item = &Generator> {
    game.ext.planets.instances.instances.values().flatten()
}

/// The save's procedural instances section (ProceduralInstancesHandler.SerializeInstances).
fn save(game: &mut Game) {
    let ids = |s: &IndexSet<i32>| s.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    game.save.procedural = game
        .ext
        .planets
        .instances
        .instances
        .iter()
        .flat_map(|(owner, list)| list.iter().map(move |g| (*owner, g)))
        .map(|(owner, g)| {
            json!({"owner": owner, "planet": g.planet, "index": g.index, "seed": g.seed as i32, "pos": floats_string(&g.position), "rot": floats_string(&g.rotation),
                   "wrecksWOGenerated": g.generated, "woIdsGenerated": ids(&g.generated_ids), "woIdsDropped": ids(&g.dropped_ids), "version": VERSION})
        })
        .collect();
}

fn save_opened(game: &mut Game) {
    let (seed, time_left) = game.ext.planets.instances.opened.as_ref().map(|o| (o.seed, o.time_left)).unwrap_or((0, 0));
    game.save.world["openedInstanceSeed"] = json!(seed);
    game.save.world["openedInstanceTimeLeft"] = json!(time_left);
}

/// ProceduralInstancesHandler.LoadInstance: a generator of the difficulty, picked by the seed.
pub fn load_instance(game: &mut Game, owner: i32, position: [f32; 3], rotation: [f32; 4], difficulty: i32, seed: i32) {
    let candidates: Vec<usize> = (0..GENERATORS.len()).filter(|i| GENERATORS[*i].1 == difficulty).collect();
    let index = if candidates.is_empty() { 0 } else { candidates[MathRandom::new(seed as u32).next_int(0, candidates.len() as i32) as usize] };
    let seed = generate::pool_seed(game, seed as u32) as i32;
    let net = spawn_generator(game, index, position, rotation);
    let planet = game.planet_hash();
    game.net.set(net, GENERATOR, "_planetHash", enc::int(planet));
    game.net.set(net, GENERATOR, "_seed", uint(seed as u32));
    game.net.set(net, GENERATOR, "_owner", enc::int(owner));
    let radius = generate::trigger_radius(game, index, owner);
    let g = Generator { net, index, planet, seed: seed as u32, generated: false, generated_ids: IndexSet::new(), dropped_ids: IndexSet::new(), position, rotation, radius };
    state(game).instances.entry(owner).or_default().push(g);
    save(game);
}

/// LoadInstanceFromSceneServerRpc: a planet's scene wreck (ProceduralSceneInstance) asks for
/// its generator the first time; the world seed and its position make the seed.
fn load_instance_from_scene(game: &mut Game, id: i32, position: [f32; 3], rotation: [f32; 4], difficulty: i32) {
    // Scene world objects have positive ids; 0 would make a fresh one.
    if id <= 0 {
        return;
    }
    if game.wos.get(&id).is_some_and(|w| !w.dont_save) {
        return;
    }
    let world_seed = game.save.settings["worldSeed"].as_u64().unwrap_or(0) as u32;
    let length = position.iter().map(|v| v * v).sum::<f32>().sqrt();
    let seed = ((world_seed as f32 * 0.5) as i32).wrapping_add(length as i32);
    if !game.wos.contains_key(&id) {
        let Some(group) = game.data.group_of_ref(game.data.singletons["ProceduralInstancesHandler"]["_proceduralInstanceGeneratorGroup"].as_str().unwrap_or("")) else { return };
        game.create_world_object(group, id, true);
    }
    game.wos.get_mut(&id).unwrap().dont_save = false;
    load_instance(game, 0, position, rotation, difficulty, seed);
}

/// GenerateNetworkObjectsServerRpc: a client entered a wreck nobody generated yet.
fn generate_network_objects(game: &mut Game, object: u64) {
    let Some(g) = generators(game).find(|g| g.net == object).cloned() else { return };
    if g.generated {
        return;
    }
    let seed = if g.seed == 0 {
        let s = game.rng.range(1, 1_073_741_823) as u32;
        let s = generate::pool_seed(game, s);
        game.net.set(object, GENERATOR, "_seed", uint(s));
        s
    } else {
        g.seed
    };
    let made = generate::generate(game, g.index, seed, g.position, g.rotation);
    let g = state(game).instances.values_mut().flatten().find(|g| g.net == object).unwrap();
    g.seed = seed;
    g.generated = true;
    g.generated_ids.extend(made);
    game.net.set(object, GENERATOR, "_networkObjectGenerated", enc::bool(true));
    save(game);
}

/// What GenerateExternalProps made goes to the owner's first generator.
fn add_generated(game: &mut Game, owner: i32, made: Vec<i32>) {
    if let Some(g) = state(game).instances.get_mut(&owner).and_then(|l| l.first_mut()) {
        g.generated_ids.extend(made);
    }
    save(game);
}

/// IsGeneratedByAnInstance.
pub fn is_generated_by_an_instance(game: &Game, id: i32) -> bool {
    generators(game).any(|g| g.generated_ids.contains(&id))
}

/// ProceduralInstancesHandler.IsInsideAnInstance (temporary instances only: the opened world
/// instance and its wrecks).
pub fn is_inside_an_instance(game: &Game, position: [f32; 3]) -> bool {
    if game.ext.planets.instances.opened.is_some() && in_instance_area(game, position) {
        return true;
    }
    game.ext.planets.instances.instances.iter().filter(|(owner, _)| **owner != 0).flat_map(|(_, list)| list).any(|g| inside_triggers(game, g, position))
}

/// CheckIfDroppedObjectIsInsideAnInstance: what is dropped in a wreck goes with the wreck.
pub fn check_dropped_object(game: &mut Game, wo: i32) {
    let Some(position) = game.wos.get(&wo).map(|w| w.position) else { return };
    let mut hits = Vec::new();
    for (owner, list) in &game.ext.planets.instances.instances {
        if let Some(i) = list.iter().position(|g| inside_triggers(game, g, position)) {
            hits.push((*owner, i));
        }
    }
    if hits.is_empty() {
        return;
    }
    for (owner, i) in hits {
        state(game).instances.get_mut(&owner).unwrap()[i].dropped_ids.insert(wo);
    }
    save(game);
}

/// DyingConsequencesHandler for a player dying inside an instance: what the instance made
/// drops where they died instead of following them.
pub fn drop_generated_objects(game: &mut Game, state: &PlayerState, position: [f32; 3]) {
    let at = [position[0], position[1] + PLAYER_HEIGHT, position[2]];
    for inventory in [state.inventory, state.equipment] {
        let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
        for wo in items.iter().rev() {
            if is_generated_by_an_instance(game, *wo) {
                game.inventories_rpc(0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(wo), json!(inventory), json!(false)]));
                game.drop_on_floor(*wo, at, [0.0, 0.0, 0.0, 1.0], 0.0, false, true, 0, 0.0, false, 0);
            }
        }
    }
}

/// CheckAndRemoveInventoryForGeneratedObjects.
fn remove_generated_items(game: &mut Game, inventory: i32) {
    if game.save.settings["dyingConsequencesLabel"].as_str() == Some("NoConsequences") {
        return;
    }
    let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
    for wo in items.iter().rev() {
        if is_generated_by_an_instance(game, *wo) {
            game.inventories_rpc(0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(wo), json!(inventory), json!(true)]));
        }
    }
}

/// PlayerStatus.GetFirstLivablePosition: the first livable (IsInsideLivable) object of the
/// planet, one meter up. ponytail: its collider's centre is not dumped; the object's position stands in.
pub fn first_livable_position(game: &Game) -> [f32; 3] {
    let planet = game.planet_hash();
    let livable = |w: &WorldObject| {
        game.data.group_prefabs[&game.data.groups[w.group].id]["components"].as_array().into_iter().flatten().any(|c| c["type"] == "HomemadeTag" && c["fields"]["homemadeTag"] == "IsInsideLivable")
    };
    game.wos.values().find(|w| w.planet_hash() == planet && livable(w)).map(|w| [w.position[0], w.position[1] + 1.0, w.position[2]]).unwrap_or([0.0; 3])
}

/// The player part of ProceduralInstancesHandler.UnloadInstance: players inside lose what the
/// instances made and respawn (in the game, saved players move to the first livable place).
fn evacuate(game: &mut Game, inside: &dyn Fn(&Game, [f32; 3]) -> bool, respawn: [f32; 3], clean_inventory: bool) {
    let players: Vec<(u64, u64, PlayerState)> = game
        .clients
        .values()
        .filter_map(|c| Some((c.id, c.player?, c.ready.clone()?)))
        .filter(|(_, object, _)| game.net.objects.get(object).is_some_and(|o| inside(game, o.position)))
        .collect();
    for (client, object, player) in players {
        if clean_inventory {
            remove_generated_items(game, player.inventory);
            remove_generated_items(game, player.equipment);
        }
        const PORTAL_CLOSED: i32 = 4;
        game.net.rpc(Target::Only(client), object, "PlayerStatus.DieAndRespawnClientRpc", &[json!(false), json!(PORTAL_CLOSED), json!(respawn)]);
    }
    let livable = first_livable_position(game);
    // ProceduralInstancesHandler: saved entries of connected players are skipped (their live
    // state above is what counts; an autosave may hold them from inside the instance).
    let online: Vec<u64> = game.clients.values().filter_map(|c| c.ready.as_ref().map(|s| s.id)).collect();
    for i in 0..game.saved_players.len() {
        let p = &game.saved_players[i];
        if p["id"].as_u64().is_some_and(|id| online.contains(&id)) {
            continue;
        }
        let position = parse_floats(p["playerPosition"].as_str().unwrap_or("")).unwrap_or([0.0; 3]);
        if !inside(game, position) {
            continue;
        }
        let (inventory, equipment) = (p["inventoryId"].as_i64().unwrap_or(0) as i32, p["equipmentId"].as_i64().unwrap_or(0) as i32);
        game.saved_players[i]["playerPosition"] = Value::String(floats_string(&livable));
        if clean_inventory {
            remove_generated_items(game, inventory);
            remove_generated_items(game, equipment);
        }
    }
}

/// Inside the opened instance scene's PreventBuilding collider.
fn in_instance_area(game: &Game, position: [f32; 3]) -> bool {
    game.ext.planets.instances.prevent_building.as_ref().is_some_and(|s| s.contains(position))
}

/// ProceduralInstancesHandler.UnloadInstance, then GenerateOnTrigger.Unload(clearNetworkObjects):
/// what the generator made or had dropped in it, still inside, goes, then the generator.
fn unload_instance(game: &mut Game, owner: i32, index: usize, respawn: [f32; 3], clean_inventory: bool) {
    let g = game.ext.planets.instances.instances[&owner][index].clone();
    evacuate(game, &|game, p| inside_triggers(game, &g, p), respawn, clean_inventory);
    for (list, check_inventory) in [(&g.generated_ids, false), (&g.dropped_ids, true)] {
        for id in list {
            let inside = game.wos.get(id).and_then(|w| Some((w.net?, w.linked_inventory)));
            if let Some((net, inventory)) = inside {
                let at = game.net.objects.get(&net).map(|o| o.position).unwrap_or([0.0; 3]);
                if inside_triggers(game, &g, at) && (!check_inventory || inventory == 0) {
                    game.destroy_world_object(*id, true);
                }
            }
        }
    }
    if g.generated {
        game.net.set(g.net, GENERATOR, "_networkObjectGenerated", enc::bool(false));
    }
    let due = game.time + DESPAWN_DELAY;
    let s = state(game);
    s.despawns.push((due, g.net));
    s.instances.get_mut(&owner).unwrap().remove(index);
    save(game);
}

/// ProceduralInstancesHandler.UnloadInstances.
fn unload_instances(game: &mut Game, owner: i32, respawn: [f32; 3]) {
    if !game.ext.planets.instances.instances.contains_key(&owner) {
        return;
    }
    while game.ext.planets.instances.instances[&owner].first().is_some() {
        unload_instance(game, owner, 0, respawn, true);
    }
    state(game).instances.shift_remove(&owner);
    save(game);
}

// ---- world instances -----------------------------------------------------------------------

fn portal_generators(game: &Game) -> Vec<&WorldObject> {
    let Some(group) = game.data.group("PortalGenerator1") else { return Vec::new() };
    game.wos.values().filter(|w| w.group == group && w.net.is_some()).collect()
}

/// WorldInstanceHandler.SetAndOpenWorldInstanceScene: one instance at a time; none closes it
/// (without unloading its wrecks, as in the game).
pub fn set_and_open(game: &mut Game, data: Option<WorldInstance>) {
    let s = state(game);
    if s.opened.is_some() && data.is_some() {
        return;
    }
    // NetworkVariable.Value: an equal value (WorldInstanceData equals by seed) changes nothing.
    if s.opened.as_ref().map(|o| o.seed) == data.as_ref().map(|d| d.seed) {
        return;
    }
    s.opened = data;
    let opened = s.opened.clone();
    // ReduceTimeOpenedWorldInstance starts with the scene and takes its first second at once
    // (the real game's delta carries it).
    if let Some(o) = s.opened.as_mut() {
        o.time_left = (o.time_left - 1).max(0);
    }
    let due = game.time + 1.0;
    state(game).next_second = due;
    let bytes = encode_instance(game.ext.planets.instances.opened.as_ref());
    game.set_on(HANDLER, "_openedWorldInstanceData", bytes);
    save_opened(game);
    // HandleDataAfterLoad, once the scene is loaded: InitSceneVariables generates what a newly
    // opened instance holds; SetCurrentWorldInstanceCollider.
    let scene = opened.as_ref().and_then(|o| generate::instance_scene(game, &o.scene));
    state(game).prevent_building = scene.and_then(|s| s.prevent_building.clone());
    if let Some(o) = opened.filter(|o| !o.loaded_from_db) {
        generate::init_scene_variables(game, o.seed, o.difficulty, &o.scene);
    }
}

/// WorldInstanceHandler.UnloadInstance.
pub fn unload_world_instance(game: &mut Game) {
    let Some(seed) = game.ext.planets.instances.opened.as_ref().map(|o| o.seed) else { return };
    let planet = game.planet_hash();
    let respawn = portal_generators(game).iter().filter(|w| w.planet_hash() == planet).last().map(|w| [w.position[0], w.position[1] + 7.0, w.position[2]]).unwrap_or([0.0; 3]);
    // The instance's wreck generators (whose triggers cover it) are what sends players back;
    // without any (no wreck entry of its variation is active), the instance area does it.
    if !game.ext.planets.instances.instances.contains_key(&seed) {
        evacuate(game, &|game, p| in_instance_area(game, p), respawn, true);
    }
    unload_instances(game, seed, respawn);
    set_and_open(game, None);
}

/// The opened instance's loot rarity (InventoryFromScene.hasWorldInstanceLoot).
pub fn opened_loot_rarity(game: &Game) -> Option<i32> {
    game.ext.planets.instances.opened.as_ref().map(|o| o.loot_rarity)
}

fn open_from_rpc(game: &mut Game, a: &Args) {
    // [seed, time left, scene, loot rarity, difficulty, recipe] or null (rpcspec decodes it).
    let data = a.0.first().and_then(|v| v.as_array()).map(|d| {
        let int = |i: usize| d.get(i).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
        let recipe = d.get(5).and_then(|r| r.as_array()).map(|r| r.iter().filter_map(|x| x.as_i64()).map(|x| x as i32).collect()).unwrap_or_default();
        WorldInstance { seed: int(0), time_left: int(1), scene: d.get(2).and_then(|x| x.as_str()).unwrap_or("").to_string(), loot_rarity: int(3), difficulty: int(4), recipe, loaded_from_db: false }
    });
    set_and_open(game, data);
}

// ---- save ----------------------------------------------------------------------------------

/// ProceduralInstancesHandler.DeserializeInstances; false when the save's generators are of
/// another version (they are then all unloaded).
fn deserialize(game: &mut Game, instance_seed: i32) -> bool {
    let mut outdated = false;
    let int = |v: &Value| v.as_i64().unwrap_or(0) as i32;
    let ids = |v: &Value| -> IndexSet<i32> { v.as_str().unwrap_or("").split(',').filter_map(|s| s.parse().ok()).collect() };
    for d in game.save.procedural.clone() {
        let index = int(&d["index"]) as usize;
        if index >= GENERATORS.len() {
            continue;
        }
        let position = parse_floats(d["pos"].as_str().unwrap_or("")).unwrap_or([0.0; 3]);
        let rotation = parse_floats(d["rot"].as_str().unwrap_or("")).unwrap_or([0.0, 0.0, 0.0, 1.0]);
        let net = spawn_generator(game, index, position, rotation);
        let planet = if int(&d["planet"]) == 0 { game.planet_hash() } else { int(&d["planet"]) };
        game.net.set(net, GENERATOR, "_planetHash", enc::int(planet));
        // InitGeneratedNetworkObjects: a wreck safe among them means it was generated. One
        // marked generated with nothing in it (saves of this server from before it generated
        // wrecks) is generated again.
        let generated_ids = ids(&d["woIdsGenerated"]);
        let safe = game.data.group("ProceduralWreckSafe");
        let generated = (d["wrecksWOGenerated"].as_bool() == Some(true) && !generated_ids.is_empty()) || generated_ids.iter().any(|id| game.wos.get(id).is_some_and(|w| Some(w.group) == safe));
        game.net.set(net, GENERATOR, "_networkObjectGenerated", enc::bool(generated));
        for id in &generated_ids {
            // GroupNetworkBase.SetWorldObjectFromGeneratedWreck: wreck buildings do not count.
            if game.wos.get(id).is_some_and(|w| w.net.is_some() && game.data.groups[w.group].constructible) {
                game.constructed.shift_remove(id);
            }
        }
        for p in game.units.planets.iter().map(|p| p.hash).collect::<Vec<_>>() {
            game.units.force_reset(p);
        }
        // A wreck not generated yet takes a seed the server has the layout of.
        let seed = int(&d["seed"]) as u32;
        let seed = if generated { seed } else { generate::pool_seed(game, seed) };
        game.net.set(net, GENERATOR, "_seed", uint(seed));
        let owner = if d["version"].as_i64().unwrap_or(0) < 3 { instance_seed } else { int(&d["owner"]) };
        game.net.set(net, GENERATOR, "_owner", enc::int(owner));
        let radius = generate::trigger_radius(game, index, owner);
        let g = Generator { net, index, planet, seed, generated, generated_ids, dropped_ids: ids(&d["woIdsDropped"]), position, rotation, radius };
        state(game).instances.entry(owner).or_default().push(g);
        outdated |= d["version"].as_i64() != Some(VERSION);
    }
    let generator_group = game.data.group_of_ref(game.data.singletons["ProceduralInstancesHandler"]["_proceduralInstanceGeneratorGroup"].as_str().unwrap_or(""));
    // RemoveAllGeneratorsWOs: the scene wrecks' world objects go, so they load again.
    let remove_generator_wos = |game: &mut Game| game.wos.retain(|_, w| Some(w.group) != generator_group);
    if outdated {
        for owner in game.ext.planets.instances.instances.keys().copied().collect::<Vec<_>>() {
            for i in (0..game.ext.planets.instances.instances[&owner].len()).rev() {
                unload_instance(game, owner, i, [0.0; 3], false);
            }
        }
        remove_generator_wos(game);
        return false;
    }
    let scene_wrecks = game.ext.planets.instances.instances.get(&0).is_some_and(|l| !l.is_empty());
    if !scene_wrecks && generator_group.is_some_and(|g| game.first_world_object_of_group(g, 0).is_some()) {
        remove_generator_wos(game);
    }
    true
}

pub fn init(game: &mut Game) {
    state(game).instances.insert(0, Vec::new());
    let seed = game.save.world["openedInstanceSeed"].as_i64().unwrap_or(0) as i32;
    let time_left = game.save.world["openedInstanceTimeLeft"].as_i64().unwrap_or(0) as i32;
    // An opened instance reopens with the generators and props the save has (loaded from db).
    if deserialize(game, seed) && seed != 0 {
        let data = instance_via_seed(game, seed, time_left, true);
        set_and_open(game, data);
    }
    save(game);
}

pub fn tick(game: &mut Game) {
    let now = game.time;
    let s = state(game);
    let due: Vec<u64> = s.despawns.iter().filter(|(t, _)| *t <= now).map(|(_, n)| *n).collect();
    s.despawns.retain(|(t, _)| *t > now);
    let mut ticked = false;
    if let Some(o) = s.opened.as_mut().filter(|_| now >= s.next_second) {
        // In place, as the game does: no delta, but late joiners get the time left.
        o.time_left = (o.time_left - 1).max(0);
        s.next_second += 1.0;
        ticked = true;
    }
    for net in due {
        game.net.despawn(net);
    }
    if ticked {
        let bytes = encode_instance(game.ext.planets.instances.opened.as_ref());
        if let Some((object, _)) = game.net.find_class(HANDLER) {
            game.net.objects.get_mut(&object).unwrap().init(HANDLER, "_openedWorldInstanceData", bytes);
        }
        save_opened(game);
    }
}

pub fn rpc(game: &mut Game, _client: u64, object: u64, name: &str, a: &Args) -> bool {
    match name {
        "WorldInstanceHandler.UpdateChoicesServerRpc" => {
            // The portal window asks for 3 (UiWindowPortalGenerator._numberOfInstancesToShow); a
            // client asking for millions would make the server allocate them.
            let count = a.i(0).clamp(0, 16);
            let choices: Vec<(i32, i32)> = (0..count).map(|_| (game.rng.range(1, i32::MAX), game.rng.range(15 * 60, 35 * 60))).collect();
            game.set_on(HANDLER, "_currentChoices", encode_choices(&choices));
            state(game).choices = choices;
        }
        "WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc" => open_from_rpc(game, a),
        "WorldInstanceHandler.UnloadInstanceServerRpc" => unload_world_instance(game),
        "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc" => load_instance_from_scene(game, a.i(0), a.v3(1), a.q(2), a.i(3)),
        "GenerateOnTrigger.GenerateNetworkObjectsServerRpc" => generate_network_objects(game, object),
        _ => return false,
    }
    true
}

/// WorldInstanceHandler.UnregisterMachinePortalGenerator: the last portal generator gone
/// closes the opened instance.
pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    if game.data.groups[wo.group].id == "PortalGenerator1" && wo.net.is_some() && portal_generators(game).is_empty() {
        unload_world_instance(game);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::hex;

    fn prime_instance(seed: i32) -> Option<WorldInstance> {
        let g = crate::planets::tests::game()?;
        Some(instance_via_seed(&g, seed, 1200, false).unwrap())
    }

    #[test]
    fn world_instance_data_serializes_like_the_game() {
        // _openedWorldInstanceData as the real game sent it, opened with seed 400000032 and
        // 1200 s (its clock had taken the first second already).
        let Some(mut opened) = prime_instance(400_000_032) else { return };
        assert_eq!((opened.scene.as_str(), opened.loot_rarity, opened.difficulty), ("Instance-HugeTrees", 0, 0));
        opened.time_left = 1199;
        let real = "2084d717af0400001200000049006e007300740061006e00630065002d00480075006700650054007200650065007300000000000000000003000000380a988c380a988c380a988c";
        assert_eq!(encode_instance(Some(&opened)), hex(real));
        assert_eq!(encode_instance(None), vec![0, 0, 0, 0]);
        let other = prime_instance(123_456_789).unwrap();
        assert_eq!((other.scene.as_str(), other.loot_rarity, other.difficulty), ("Instance-CargoSpaceShip", 3, 2));
        assert_eq!(other.recipe.len(), 7);
        // _currentChoices.
        assert_eq!(encode_choices(&[(0x7670_3fd9, 1478), (0x7deb_803e, 1894), (0x7771_9135, 983)]), hex("03000000d93f70763e80eb7d3591717703000000c605000066070000d7030000"));
    }

    #[test]
    fn scene_wrecks_spawn_a_generator_and_are_saved() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        g.net.out.clear();
        rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(101000001), json!([300.0, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(1)]));
        let generator = g.ext.planets.instances.instances[&0][0].clone();
        assert_eq!(generator.index, 1);
        // The game's seed for it (world seed and position), as one this server has the layout of.
        let seed = generate::pool_seed(&mut g, 310087328 + 500);
        assert_eq!(generator.seed, seed);
        // Spawned as the real game spawns it (defaults), the values follow as a delta.
        let create = g.net.out.iter().find(|(_, kind, _)| *kind == crate::protocol::CREATE_OBJECT).unwrap();
        let o = crate::protocol::read_scene_object(&mut crate::wire::Reader::new(&create.2[1..])).unwrap();
        assert_eq!((o.hash, o.flags, o.sync), (0x42d7ff3e, 0x68, hex("0109000100")));
        g.net.flush();
        let delta = g.net.out.iter().rev().find(|(_, kind, _)| *kind == crate::protocol::NETWORK_VARIABLE_DELTA).unwrap();
        let mut w = Writer::new();
        w.packed_u64(generator.net).packed_u16(0).i32(4).bool(true).packed_u32(seed).bool(true).packed_i32(0).bool(false).bool(true).packed_i32(-1140328421);
        assert_eq!(delta.2, w.0);
        assert!(!g.wos[&101000001].dont_save);
        assert_eq!(g.save.procedural.len(), 1);
        assert_eq!(g.save.procedural[0]["seed"], json!(seed));
        // The same wreck again: already loaded.
        rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(101000001), json!([300.0, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(1)]));
        assert_eq!(g.ext.planets.instances.instances[&0].len(), 1);
        rpc(&mut g, 1, generator.net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        assert!(g.ext.planets.instances.instances[&0][0].generated);
        assert_eq!(g.save.procedural[0]["wrecksWOGenerated"], json!(true));
        // Saved and loaded again: the generator comes back.
        let saved = g.save.procedural.clone();
        let mut h = crate::planets::tests::game().unwrap();
        h.save.procedural = saved;
        h.ext.planets.instances = Default::default();
        init(&mut h);
        let back = &h.ext.planets.instances.instances[&0][0];
        assert_eq!((back.seed, back.index, back.generated, back.position), (generator.seed, 1, true, [300.0, 10.0, 400.0]));
    }

    #[test]
    fn wrecks_generate_what_the_game_generates_from_the_seed() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        // The real game (2.103) generating a wreck of each size from a save, one after the other
        // (the prefabs' ignore flags carry over): how many objects it spawned, the first two and
        // the last, in order.
        let real = [
            (0, 5, [2000.0, 50.0, 2000.0], 61, [[1992.75, 49.996002, 2039.445], [1992.75, 49.996002, 2044.995], [2018.61, 50.0, 2057.13]]),
            (1, 17, [-1500.0, 50.0, 2500.0], 90, [[-1524.1422, 46.32, 2588.7944], [-1511.3379, 52.91, 2586.945], [-1526.06, 44.896, 2561.16]]),
            (2, 33, [2500.0, 50.0, -1500.0], 102, [[2592.2332, 50.52, -1470.9714], [2596.47, 50.52, -1459.47], [2572.0, 64.0, -1394.5]]),
        ];
        for (index, seed, position, count, samples) in real {
            let made = generate::generate(&mut g, index, seed, position, [0.0, 0.0, 0.0, 1.0]);
            let spawned: Vec<[f32; 3]> = made.iter().filter(|id| g.wos[*id].net.is_some()).map(|id| g.wos[id].position).collect();
            assert_eq!(spawned.len(), count, "wreck {index}");
            let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 0.001);
            assert!(close(spawned[0], samples[0]) && close(spawned[1], samples[1]) && close(spawned[count - 1], samples[2]), "wreck {index}: {:?}", &spawned[..2]);
            // What the containers hold is generated with them.
            assert!(made.len() > count);
        }
        assert_eq!(generate::pool_seed(&mut g, 64), 64);
        assert_eq!(generate::pool_seed(&mut g, 310087828), 21);
    }

    #[test]
    fn empty_wrecks_of_older_saves_get_a_known_seed_and_are_generated_again() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        g.save.procedural = vec![json!({"owner": 0, "planet": -1140328421, "index": 0, "seed": 310087828, "pos": "2000,50,2000", "rot": "0,0,0,1",
                                        "wrecksWOGenerated": true, "woIdsGenerated": "", "woIdsDropped": "", "version": VERSION})];
        init(&mut g);
        let wreck = g.ext.planets.instances.instances[&0][0].clone();
        assert_eq!((wreck.generated, wreck.seed), (false, 21));
        rpc(&mut g, 1, wreck.net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        let wreck = &g.ext.planets.instances.instances[&0][0];
        assert!(wreck.generated && !wreck.generated_ids.is_empty());
        assert_eq!(g.save.procedural[0]["woIdsGenerated"].as_str().unwrap().split(',').count(), wreck.generated_ids.len());
    }

    #[test]
    fn opened_instances_get_their_wreck_generators_and_props_as_in_the_game() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        set_and_open(&mut g, prime_instance(400_000_032));
        // The real game (2.103) opening this instance: its variation keeps one of the two wreck
        // entries, and seven of the nine loot containers come out (ProceduralWreckContainer1).
        let generators = &g.ext.planets.instances.instances[&400_000_032];
        assert_eq!(generators.len(), 1);
        assert_eq!((generators[0].index, generators[0].position), (0, [1239.3069, 67.828445, 4634.242]));
        let made: Vec<[f32; 3]> = generators[0].generated_ids.iter().filter(|id| g.data.groups[g.wos[*id].group].id == "ProceduralWreckContainer1").map(|id| g.wos[id].position).collect();
        let real = [[1358.8, 150.61864, 4260.8], [1148.28, 136.61421, 4412.18], [1212.5, 135.93988, 4306.7], [1449.8, 155.49957, 4448.5], [1361.7, 145.846, 4532.0], [1321.5, 137.4616, 4554.7], [1135.6, 135.43, 4272.6]];
        assert_eq!(made, real);
        // Their loot is theirs too: what the player carries out of the instance goes with it.
        assert!(generators[0].generated_ids.len() > made.len());
        assert!(is_inside_an_instance(&g, [1143.0, 268.0, 4400.0]));
    }

    /// The portal opens an instance by RPC: everyone's variable holds it, the save too, and a
    /// reloaded world has it open still.
    #[test]
    fn portals_open_world_instances_by_rpc_and_the_save_keeps_them() {
        use crate::testkit::*;
        let Some(mut g) = crate::planets::tests::game() else { return };
        join(&mut g, 1, "Ann");
        let a = prime_instance(400_000_032).unwrap();
        let data = json!([a.seed, a.time_left, a.scene, a.loot_rarity, a.difficulty, a.recipe]);
        send_scene(&mut g, 1, "WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc", &[data]);
        let opened = g.ext.planets.instances.opened.clone().expect("opened");
        assert_eq!((opened.seed, opened.time_left, opened.scene.as_str()), (a.seed, a.time_left - 1, a.scene.as_str()));
        let (handler, _) = g.net.find_class(HANDLER).unwrap();
        assert_eq!(var(&g, handler, HANDLER, "_openedWorldInstanceData"), encode_instance(Some(&opened)));
        assert_eq!(g.save.world["openedInstanceSeed"], json!(a.seed));
        let g = reload(&mut g);
        assert_eq!(g.ext.planets.instances.opened.as_ref().map(|o| o.seed), Some(a.seed));
    }

    #[test]
    fn world_instances_open_once_count_down_and_close() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        rpc(&mut g, 1, 0, "WorldInstanceHandler.UpdateChoicesServerRpc", &Args(vec![json!(3)]));
        let choices = g.ext.planets.instances.choices.clone();
        assert_eq!(choices.len(), 3);
        assert!(choices.iter().all(|(s, t)| *s >= 1 && (900..2100).contains(t)));
        let a = prime_instance(400_000_032).unwrap();
        set_and_open(&mut g, Some(a.clone()));
        set_and_open(&mut g, prime_instance(987_654_321));
        assert_eq!(g.ext.planets.instances.opened.as_ref().map(|o| o.seed), Some(a.seed), "one at a time");
        assert_eq!(g.save.world["openedInstanceSeed"], json!(400_000_032));
        g.time += 0.01;
        tick(&mut g);
        g.time += 1.0;
        tick(&mut g);
        assert_eq!(g.save.world["openedInstanceTimeLeft"], json!(1198));
        assert!(is_inside_an_instance(&g, [1239.3, 67.8, 4634.2]));
        assert!(!is_inside_an_instance(&g, [558.2, 2.47, 494.1]));
        // Someone who left the game inside the instance is back at the escape pod after it.
        g.saved_players.push(json!({"id": 7, "name": "Away", "inventoryId": 900, "equipmentId": 901, "playerPosition": "1240,70,4630"}));
        rpc(&mut g, 1, 0, "WorldInstanceHandler.UnloadInstanceServerRpc", &Args(vec![]));
        assert_eq!(g.saved_players.last().unwrap()["playerPosition"], json!("558.2,3.47,494.1"));
        assert!(g.ext.planets.instances.opened.is_none());
        assert_eq!(g.save.world["openedInstanceSeed"], json!(0));
        assert!(!is_inside_an_instance(&g, [1239.3, 67.8, 4634.2]));
    }
}
