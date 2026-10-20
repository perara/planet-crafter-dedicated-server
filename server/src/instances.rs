//! SpaceCraft.ProceduralInstancesHandler, GenerateOnTrigger and WorldInstanceHandler, server
//! side: procedural wrecks (a generator network object; clients build the wreck from its seed)
//! and the world instances portals open, with the save's procedural instances and the world
//! state's opened instance.
//!
//! What a wreck and an opened world instance hold is generated in generate.rs.
//!
//! A wreck is laid out by Tessera from its own seed, as in the game. That takes up to a few
//! seconds, so it runs on a thread between the walk before it and the walk after it
//! (generate.rs), the wrecks one at a time in the order they were asked for, as the game
//! generates them (ProceduralInstancesHandler's generating count): GenerateNetworkObjects
//! queues one, tick finishes it, and the tick itself never waits for the solver.

pub mod generate;

use std::collections::{HashSet, VecDeque};
use std::sync::{mpsc, Mutex};

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};

use crate::clock::GameTime;
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
/// How many wrecks may wait for their layout at once. The game has no limit; a client that asked
/// for a hundred (each one a few seconds of solver) would keep a core busy for minutes. Players
/// in range of a wreck are the only ones to ask, so this is more than a server ever sees.
const MAX_QUEUE: usize = 32;
/// How many scene wrecks (owner 0) the world may hold. A planet's scene has a few dozen at most (a
/// late-game Prime save: 8); a client may ask for any number of them, before PlayerReady too, and
/// each is a network object for everyone, a world object and a line of the save.
const MAX_SCENE_WRECKS: usize = 256;
/// The most of a scene name and of a recipe a client's SetAndOpenWorldInstanceScene may carry
/// (the scenes' names are 23 characters at most, recipes 7 entries).
const MAX_SCENE_NAME: usize = 64;
const MAX_RECIPE: usize = 64;
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
    next_second: GameTime,
    /// Generators to despawn: (game time, network object).
    despawns: Vec<(GameTime, u64)>,
    /// RandomizePropData.ignore of the wreck props (the game keeps it on the prefabs), as the
    /// generations done so far left it: a wreck's walk starts from it.
    ignored: HashSet<(String, usize)>,
    /// The generators (network objects) asked to generate their wreck, in order; the first
    /// one is the one being laid out.
    queue: VecDeque<u64>,
    /// The first one's Tessera, running: not in a snapshot, started again from `ignored` and
    /// the generator's seed by the process that restores it.
    #[serde(skip)]
    job: Option<Job>,
    /// The opened instance scene's PreventBuilding collider.
    prevent_building: Option<generate::Shape>,
}

/// A wreck's Tessera on its thread.
struct Job {
    net: u64,
    index: usize,
    seed: u32,
    position: [f32; 3],
    rotation: [f32; 4],
    prepared: generate::Prepared,
    /// (A Mutex only so that Game stays Sync.)
    result: Mutex<mpsc::Receiver<Result<generate::Layout, String>>>,
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
    // list.Sort(y.id.CompareTo(x.id)): an introsort; equal ids are the same ingredient twice.
    let _ = crate::dotnet_sort::sort_by(&mut ids, |x, y| crate::inventories::culture_cmp(y, x));
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
fn load_instance_from_scene(game: &mut Game, client: u64, id: i32, position: [f32; 3], rotation: [f32; 4], difficulty: i32) {
    // Scene world objects have positive ids below the players' (FIRST_PLAYER_MADE_ID: the ones from
    // there on are handed out as things are made); 0 would make a fresh one.
    if id <= 0 || !crate::model::is_from_scene(id) {
        return;
    }
    if game.wos.get(&id).is_some_and(|w| !w.dont_save) {
        return;
    }
    if state(game).instances.get(&0).is_some_and(|l| l.len() >= MAX_SCENE_WRECKS) {
        game.limits.warn(client, "more scene wrecks than a planet has asked for, ignored");
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

/// GenerateNetworkObjectsServerRpc: a client entered a wreck nobody generated yet. It joins the
/// queue; `pump` lays it out and creates what it holds.
fn generate_network_objects(game: &mut Game, object: u64) {
    let Some(g) = generators(game).find(|g| g.net == object).cloned() else { return };
    if g.generated || state(game).queue.contains(&object) {
        return;
    }
    if state(game).queue.len() >= MAX_QUEUE {
        eprintln!("wreck {}: {MAX_QUEUE} wrecks already wait for their layout; ignored", g.index);
        return;
    }
    if g.seed == 0 {
        let seed = game.rng.range(1, 1_073_741_823) as u32;
        game.net.set(object, GENERATOR, "_seed", uint(seed));
        state(game).instances.values_mut().flatten().find(|g| g.net == object).unwrap().seed = seed;
    }
    state(game).queue.push_back(object);
}

/// Starts the first queued wreck's Tessera when none runs, and creates the wreck of the one
/// that has finished (GenerateOnTrigger.Generate after the solver).
fn pump(game: &mut Game) {
    let polled = state(game).job.as_ref().map(|job| job.result.lock().unwrap().try_recv());
    match polled {
        Some(Err(mpsc::TryRecvError::Empty)) => return,
        Some(answer) => {
            let job = state(game).job.take().unwrap();
            state(game).queue.pop_front();
            match answer {
                Ok(result) => complete(game, job, result),
                Err(_) => eprintln!("wreck {} (seed {}): the Tessera thread died", job.index, job.seed),
            }
        }
        None => {}
    }
    while state(game).job.is_none() {
        let Some(net) = state(game).queue.front().copied() else { return };
        let Some(g) = generators(game).find(|g| g.net == net && !g.generated).cloned() else {
            state(game).queue.pop_front();
            continue;
        };
        let ignored = state(game).ignored.clone();
        let Some(prepared) = generate::prepare(game, g.index, g.seed, g.position, g.rotation, ignored) else {
            state(game).queue.pop_front();
            continue;
        };
        let (tx, result) = mpsc::channel();
        let (wrecks, scene, index, seed) = (generate::wrecks(game), prepared.scene.clone(), g.index, g.seed);
        let spawned = std::thread::Builder::new().name("pcds-tessera".into()).spawn(move || {
            let _ = tx.send(generate::solve(&wrecks, index, seed, &scene));
        });
        match spawned {
            Ok(_) => state(game).job = Some(Job { net, index, seed, position: g.position, rotation: g.rotation, prepared, result: Mutex::new(result) }),
            Err(e) => {
                eprintln!("wreck {index} (seed {seed}): no thread for Tessera: {e}");
                state(game).queue.pop_front();
            }
        }
    }
}

/// A wreck's layout is in: what Generate makes of it, saved with the generator.
fn complete(game: &mut Game, job: Job, result: Result<generate::Layout, String>) {
    let layout = match result {
        Ok(layout) => layout,
        Err(e) => return eprintln!("wreck {} (seed {}): {e}", job.index, job.seed),
    };
    let (made, ignored) = generate::finish(game, job.index, job.seed, job.position, job.rotation, job.prepared, &layout);
    state(game).ignored = ignored;
    if let Some(g) = state(game).instances.values_mut().flatten().find(|g| g.net == job.net) {
        g.generated = true;
        g.generated_ids.extend(made);
    }
    game.net.set(job.net, GENERATOR, "_networkObjectGenerated", enc::bool(true));
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
    // A wreck unloaded while it is laid out is not made: its thread's answer is dropped.
    s.queue.retain(|n| *n != g.net);
    if s.job.as_ref().is_some_and(|j| j.net == g.net) {
        s.job = None;
    }
    s.despawns.push((due, g.net));
    s.instances.get_mut(&owner).unwrap().remove(index);
    save(game);
}

/// ProceduralInstancesHandler.UnloadInstances.
fn unload_instances(game: &mut Game, owner: i32, respawn: [f32; 3]) {
    if !game.ext.planets.instances.instances.contains_key(&owner) {
        return;
    }
    while !game.ext.planets.instances.instances[&owner].is_empty() {
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
    let respawn = portal_generators(game).iter().rfind(|w| w.planet_hash() == planet).map(|w| [w.position[0], w.position[1] + 7.0, w.position[2]]).unwrap_or([0.0; 3]);
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
    // The data goes whole to every client and stays: a scene is named in 23 characters at most
    // and a recipe lists a handful of props.
    if data.as_ref().is_some_and(|d| d.scene.len() > MAX_SCENE_NAME || d.recipe.len() > MAX_RECIPE) {
        eprintln!("world instance: a scene name or recipe over {MAX_SCENE_NAME} bytes or {MAX_RECIPE} entries; ignored");
        return;
    }
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
                game.constructed.remove(id);
            }
        }
        for p in game.units.planets.iter().map(|p| p.hash).collect::<Vec<_>>() {
            game.units.force_reset(p);
        }
        let seed = int(&d["seed"]) as u32;
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
    pump(game);
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

pub fn rpc(game: &mut Game, client: u64, object: u64, name: &str, a: &Args) -> bool {
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
        "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc" => load_instance_from_scene(game, client, a.i(0), a.v3(1), a.q(2), a.i(3)),
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

    /// Ticks until the wrecks asked for are laid out (their Tessera runs on a thread).
    fn settle(g: &mut Game) {
        for _ in 0..20_000 {
            tick(g);
            if g.ext.planets.instances.queue.is_empty() && g.ext.planets.instances.job.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the wrecks were not laid out in 40 s");
    }

    fn prime_instance(seed: i32) -> Option<WorldInstance> {
        let g = crate::planets::tests::game()?;
        Some(instance_via_seed(&g, seed, 1200, false).unwrap())
    }

    /// The portals of a DLC planet open every scene its PlanetData lists, with the planet's own
    /// dump (Humble's lagoon, Toxicity's landslide among them): each scene is picked by some seed
    /// and opens with its wrecks and props. Real DLC data: skipped, loudly, without it.
    #[test]
    fn the_portals_of_the_dlc_planets_open_all_their_scenes() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let grounds = crate::testkit::dlc_grounds(&g, "the_portals_of_the_dlc_planets_open_all_their_scenes");
        for (planet, scene) in grounds {
            g.planet = g.net_data.planet_index(&planet).unwrap();
            let listed: Vec<String> = {
                let data = g.data.asset(&format!("PlanetData:PlanetData-{planet}")).unwrap();
                let names = g.data.asset(data["instanceScenesData"].as_str().unwrap()).unwrap()["instanceSceneNames"].as_array().unwrap().clone();
                names.iter().map(|n| n.as_str().unwrap().to_string()).collect()
            };
            assert_eq!(listed.iter().filter(|n| !scene.instance_scenes.contains_key(*n)).count(), 0, "{planet}: a listed scene is not in the dump");
            let mut opened = std::collections::HashSet::new();
            for seed in 1..3000 {
                let instance = instance_via_seed(&g, seed, 1200, false).expect("an instance");
                if opened.insert(instance.scene.clone()) {
                    let scene = instance.scene.clone();
                    set_and_open(&mut g, Some(instance));
                    assert_eq!(g.ext.planets.instances.opened.as_ref().map(|o| o.scene.clone()), Some(scene));
                    unload_world_instance(&mut g);
                }
            }
            let mut want: Vec<_> = listed.clone();
            want.sort();
            want.dedup();
            let mut got: Vec<_> = opened.into_iter().collect();
            got.sort();
            assert_eq!(got, want, "{planet}: every scene its PlanetData lists is picked by some seed");
        }
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
        // The game's seed for it: the world seed and the position.
        let seed = 310087328 + 500;
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
        assert!(!g.ext.planets.instances.instances[&0][0].generated, "laid out between ticks");
        settle(&mut g);
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
            let made = generate::generate_now(&mut g, index, seed, position, [0.0, 0.0, 0.0, 1.0]);
            let spawned: Vec<[f32; 3]> = made.iter().filter(|id| g.wos[*id].net.is_some()).map(|id| g.wos[id].position).collect();
            assert_eq!(spawned.len(), count, "wreck {index}");
            let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 0.001);
            assert!(close(spawned[0], samples[0]) && close(spawned[1], samples[1]) && close(spawned[count - 1], samples[2]), "wreck {index}: {:?}", &spawned[..2]);
            // What the containers hold is generated with them.
            assert!(made.len() > count);
        }
    }

    /// Wrecks as the game lays them out from seeds that are not any pool's: the real game (2.103) hosting
    /// a fresh world, six wrecks one after the other at these places (scene wrecks seed themselves from
    /// the world seed and the place): how many network objects it spawned, the first two and the last.
    #[test]
    fn wrecks_of_any_seed_come_out_as_the_game_lays_them() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let real = [
            (0, 843016137, [282.918, 61.804, -1843.231], [0.0, 0.0, 0.0, 1.0], 43, [[326.7124, 58.124, -1750.0889], [324.8632, 64.714, -1762.8931], [319.858, 67.787, -1784.574]]),
            (1, 843014640, [319.715, 30.836, 180.666], [0.0, 0.7071068, 0.0, -0.7071068], 76, [[290.6864, 31.356, 272.8992], [279.185, 31.356, 277.136], [214.215, 32.836, 252.666]]),
            (2, 843015679, [-964.9, -4.627, -1025.344], [0.0, 0.0, 0.0, 1.0], 91, [[-965.82, -4.627, -999.144], [-938.15, 7.373, -1010.944], [-982.35, 6.589, -937.768]]),
            (0, 843015151, [-857.188, 57.818, 188.524], [-0.007738, 0.7070644, -0.0232104, -0.7067257], 42, [[-901.7588, 57.2035, 249.7594], [-896.2141, 57.4463, 249.7594], [-945.655, 55.1259, 242.5111]]),
            (1, 843015244, [817.161, 67.852, 523.088], [0.0048036, 0.5986985, -0.0008654, 0.8009596], 73, [[871.9254, 64.4242, 426.4821], [870.1208, 70.9418, 439.3298], [938.7405, 69.2451, 483.9122]]),
            (2, 843016662, [2390.863, 1.565, -23.737], [-0.0104618, 0.7070294, 0.0034874, 0.7070982], 122, [[2416.1575, -2.6834, -131.8376], [2417.8738, 4.0678, -119.0989], [2496.0806, 16.9327, -95.8716]]),
        ];
        for (index, seed, position, rotation, count, samples) in real {
            let made = generate::generate_now(&mut g, index, seed, position, rotation);
            let spawned: Vec<[f32; 3]> = made.iter().filter(|id| g.wos[*id].net.is_some()).map(|id| g.wos[id].position).collect();
            let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 0.001);
            assert_eq!(spawned.len(), count, "wreck {index} of seed {seed}: {} made", made.len());
            assert!(close(spawned[0], samples[0]) && close(spawned[1], samples[1]) && close(spawned[count - 1], samples[2]), "wreck {index} of seed {seed}: {:?}", &spawned[..2]);
        }
        // The planters of the last three told everyone their growth: 3, 2 and 2 of them (the game's
        // GrowthProxy.SetGrowth(0) on a static grower that appears).
        let growth = crate::planets::tests::rpcs_sent(&g).into_iter().filter(|n| n == "GrowthProxy.SetCurrentGrowthClientRpc").count();
        assert_eq!(growth, 7);
    }

    #[test]
    fn empty_wrecks_of_older_saves_are_generated_again_from_their_seed() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        g.save.procedural = vec![json!({"owner": 0, "planet": -1140328421, "index": 0, "seed": 310087828, "pos": "2000,50,2000", "rot": "0,0,0,1",
                                        "wrecksWOGenerated": true, "woIdsGenerated": "", "woIdsDropped": "", "version": VERSION})];
        init(&mut g);
        let wreck = g.ext.planets.instances.instances[&0][0].clone();
        assert_eq!((wreck.generated, wreck.seed), (false, 310087828));
        rpc(&mut g, 1, wreck.net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        settle(&mut g);
        let wreck = &g.ext.planets.instances.instances[&0][0];
        assert!(wreck.generated && !wreck.generated_ids.is_empty());
        assert_eq!(g.save.procedural[0]["woIdsGenerated"].as_str().unwrap().split(',').count(), wreck.generated_ids.len());
    }

    /// A flip in the middle of a wreck's Tessera: the new process starts it again from the
    /// snapshot (the queue and the prefabs' flags), and the wreck comes out as in the old one.
    #[test]
    fn a_wreck_asked_for_before_a_flip_is_laid_out_after_it() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(101000001), json!([300.0, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(1)]));
        let net = g.ext.planets.instances.instances[&0][0].net;
        rpc(&mut g, 1, net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        tick(&mut g);
        assert!(g.ext.planets.instances.job.is_some(), "the solver runs");
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        let mut h = crate::handoff::restore(net_data, data, spec, &crate::handoff::snapshot(&mut g).unwrap(), scenes).unwrap();
        assert!(h.ext.planets.instances.job.is_none() && h.ext.planets.instances.queue.len() == 1);
        settle(&mut g);
        settle(&mut h);
        let wreck = |g: &Game| -> Vec<(i32, [f32; 3])> { g.ext.planets.instances.instances[&0][0].generated_ids.iter().map(|id| (*id, g.wos[id].position)).collect() };
        assert!(h.ext.planets.instances.instances[&0][0].generated);
        assert_eq!(wreck(&g), wreck(&h));
        assert!(!wreck(&h).is_empty());
    }

    /// Wrecks are laid out one at a time, in the order they were asked for, and one unloaded
    /// while it waits is not made.
    #[test]
    fn wrecks_wait_their_turn_and_an_unloaded_one_is_not_made() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        for (id, x) in [(101000001, 300.0), (101000002, 900.0), (101000003, 1500.0)] {
            rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(id), json!([x, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(0)]));
        }
        let nets: Vec<u64> = g.ext.planets.instances.instances[&0].iter().map(|w| w.net).collect();
        for net in &nets {
            rpc(&mut g, 1, *net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
            rpc(&mut g, 2, *net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        }
        assert_eq!(g.ext.planets.instances.queue, VecDeque::from(nets.clone()), "asked twice, queued once");
        tick(&mut g);
        assert_eq!(g.ext.planets.instances.job.as_ref().map(|j| j.net), Some(nets[0]));
        // The second leaves while it waits.
        unload_instance(&mut g, 0, 1, [0.0; 3], false);
        settle(&mut g);
        let generated: Vec<u64> = g.ext.planets.instances.instances[&0].iter().filter(|w| w.generated).map(|w| w.net).collect();
        assert_eq!(generated, vec![nets[0], nets[2]]);
    }

    /// A client that asks for wreck after wreck fills a queue of a few seconds of solver each: the
    /// queue is as long as it is and no longer.
    #[test]
    fn a_client_cannot_queue_more_wrecks_than_the_cap() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        for i in 0..MAX_QUEUE + 4 {
            let id = 101_000_000 + i as i32;
            rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(id), json!([100.0 + 50.0 * i as f64, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(0)]));
            let net = g.ext.planets.instances.instances[&0][i].net;
            rpc(&mut g, 1, net, "GenerateOnTrigger.GenerateNetworkObjectsServerRpc", &Args(vec![]));
        }
        assert_eq!(g.ext.planets.instances.queue.len(), MAX_QUEUE);
    }

    /// A client (before PlayerReady too) that names wreck after wreck of the scene cannot fill the
    /// world with generators (each a network object for everyone, a world object and a line of the
    /// save), nor name ones from the players' id range.
    #[test]
    fn a_client_cannot_make_scene_wrecks_without_end() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        for id in [200_000_000, 300_000_000, i32::MAX, -5] {
            rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(id), json!([300.0, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(0)]));
        }
        assert!(g.ext.planets.instances.instances.get(&0).is_none_or(Vec::is_empty), "ids from the players' range are not the scene's");
        for i in 0..MAX_SCENE_WRECKS + 50 {
            rpc(&mut g, 1, 0, "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc", &Args(vec![json!(101_000_000 + i as i32), json!([100.0 + i as f64, 10.0, 400.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(0)]));
        }
        assert_eq!(g.ext.planets.instances.instances[&0].len(), MAX_SCENE_WRECKS);
        assert_eq!(g.save.procedural.len(), MAX_SCENE_WRECKS);
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

    /// The opened instance's data is a network variable every client is sent whole, each time one
    /// is opened: a scene name of hundreds of kilobytes (a scene's is 23 characters at most) or a
    /// recipe of as many numbers is a multiplier of the sender's bytes by the players, and by
    /// as many opens as its RPC budget allows (open, unload, open...). Only the sizes a scene has.
    #[test]
    fn an_instance_opened_by_rpc_is_the_size_of_a_scenes() {
        use crate::testkit::*;
        let Some(mut g) = crate::planets::tests::game() else { return };
        join(&mut g, 1, "Ann");
        let a = prime_instance(400_000_032).unwrap();
        for data in [json!([a.seed, a.time_left, "x".repeat(300_000), a.loot_rarity, a.difficulty, a.recipe]), json!([a.seed, a.time_left, a.scene, a.loot_rarity, a.difficulty, vec![7; 100_000]])] {
            send_scene(&mut g, 1, "WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc", &[data]);
            // (An RPC of this size is charged by its size and may wait for a later tick to run.)
            (0..100).for_each(|_| g.tick());
            assert!(g.ext.planets.instances.opened.is_none(), "an instance of a megabyte was opened");
        }
        let data = json!([a.seed, a.time_left, a.scene, a.loot_rarity, a.difficulty, a.recipe]);
        send_scene(&mut g, 1, "WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc", &[data]);
        assert!(g.ext.planets.instances.opened.is_some(), "a real one still opens");
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
