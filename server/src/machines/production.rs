//! Production: MachineGenerator (extractors, collectors, hives), MachineAutoCrafter,
//! MachineDisintegrator (ore breakers, recyclers, detoxifiers), MachineOptimizer and its
//! fuses, MachineDestructInventoryIfFull, MachineGetInRange and ConstructibleProxy's
//! RecycleContentServerRpc.

use serde_json::{json, Value};

use super::{add_new, components, dist, f32_of, force_reset, has_energy, is_active, is_full, items, remove_item, Game};
use crate::net::Target;

/// What tick runs now: mining rays, generate, craft, disintegrate.
fn due(m: &super::Machine, now: f32) -> [bool; 4] {
    [
        m.generator.as_ref().and_then(|g| g.rays.as_ref()).is_some_and(|r| now >= r.next),
        m.generator.as_ref().is_some_and(|g| !g.dead && now >= g.next),
        m.autocrafter.as_ref().is_some_and(|a| now >= a.next),
        m.disintegrator.as_ref().is_some_and(|d| now >= d.next),
    ]
}

/// The earliest time tick has anything to do (due(m, now) has a true from then on).
pub(super) fn next_due(m: &super::Machine) -> f32 {
    let g = m.generator.as_ref();
    [g.and_then(|g| g.rays.as_ref()).map(|r| r.next), g.filter(|g| !g.dead).map(|g| g.next), m.autocrafter.as_ref().map(|a| a.next), m.disintegrator.as_ref().map(|d| d.next)]
        .into_iter()
        .flatten()
        .fold(f32::INFINITY, f32::min)
}

pub fn tick(game: &mut Game, wo: i32, now: f32) {
    let Some(m) = game.ext.machines.list.get(&wo) else { return };
    let [cast, generate, craft, disintegrate] = due(m, now);
    let speed = game.rules.speed.machines;
    if cast {
        mining_rays(game, wo, now);
    }
    if generate {
        let g = game.ext.machines.get_mut(&wo).unwrap().generator.as_mut().unwrap();
        g.next = now + g.period() / speed;
        try_to_generate(game, wo);
    }
    if craft {
        let a = game.ext.machines.get_mut(&wo).unwrap().autocrafter.as_mut().unwrap();
        a.next = now + a.period / speed;
        try_to_craft(game, wo);
    }
    if disintegrate {
        let d = game.ext.machines.get_mut(&wo).unwrap().disintegrator.as_mut().unwrap();
        d.next = now + d.period / speed;
        if has_energy(game, wo) {
            try_to_disintegrate(game, wo);
        }
    }
}

/// MachineGetInRange.GetGroupsInRange: the spawned world objects within `range`.
// ponytail: an active machine asks the physics for colliders in a sphere (itself included);
// the server compares world object positions, so big buildings count from their pivot.
fn in_range(game: &Game, wo: i32, range: f32) -> Vec<i32> {
    let Some(me) = game.wos.get(&wo) else { return Vec::new() };
    let (planet, position, active) = (me.planet_hash(), me.position, is_active(game, wo));
    game.wos.values().filter(|w| (active || w.id != wo) && w.net.is_some() && w.planet_hash() == planet && dist(position, w.position) < range).map(|w| w.id).collect()
}

fn by_distance(game: &Game, wo: i32, mut list: Vec<i32>) -> Vec<i32> {
    let p = game.wos[&wo].position;
    list.sort_by(|a, b| dist(game.wos[a].position, p).total_cmp(&dist(game.wos[b].position, p)));
    list
}

// ---- MachineGenerator --------------------------------------------------------------------

pub struct Generator {
    pub next: f32,
    spawn_every: f32,
    speed: f32,
    /// groupDatas, with the groups the mining rays found.
    groups: Vec<usize>,
    terra_groups: Vec<usize>,
    terra_stage: String,
    via_linked: bool,
    when_vein_empty: bool,
    /// The TryToCastRay coroutine (None: no mining rays, or cast once for good).
    rays: Option<Rays>,
    /// The coroutine died (its vein world object is gone: a NullReferenceException in the game).
    pub dead: bool,
}

impl Generator {
    /// SetGeneratorInventory: TryToGenerate every spawnEveryXSec, and TryToCastRay. `cast`: the
    /// rays were cast already (veins::cast, for a machine on the current planet).
    pub fn new(game: &Game, f: &Value, wo: i32, now: f32, cast: bool) -> Generator {
        let refs = |k: &str| f[k].as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).collect::<Vec<usize>>();
        let spawn_every = f["spawnEveryXSec"].as_f64().unwrap_or(0.0) as f32;
        let mut g = Generator {
            next: now + spawn_every,
            spawn_every,
            speed: 1.0,
            groups: refs("groupDatas"),
            terra_groups: refs("groupDatasTerraStage"),
            terra_stage: f["terraStageNameInPlanetData"].as_str().unwrap_or("").to_string(),
            via_linked: f["setGroupsDataViaLinkedGroup"].as_bool().unwrap_or(false),
            when_vein_empty: f["generateWhenVeinIsEmpty"].as_bool().unwrap_or(false),
            rays: None,
            dead: false,
        };
        let update = f["updateMiningRaysWhenChangingTerraStage"].as_bool() == Some(true);
        if f["miningRays"].as_i64().unwrap_or(0) > 0 {
            if cast {
                let veins = g.vein_groups(game, wo);
                g.groups.extend(veins);
            }
            if !cast || update {
                g.rays = Some(Rays { next: now + RAY_CHECK_EVERY, cast, stage: crate::meteo::current_global_stage(game), update, pending: false });
            }
        }
        g
    }

    /// GetGenerationSpeed: Mathf.Round rounds halves to even.
    pub fn period(&self) -> f32 {
        (self.spawn_every / self.speed).round_ties_even()
    }

    /// What SetMiningRayGeneration adds to groupDatas: every vein group the rays found
    /// (miningRays times), which veins::cast made the linked groups (on a planet without veins,
    /// the ones the save kept). The game never takes them out of groupDatas again.
    fn vein_groups(&self, game: &Game, wo: i32) -> Vec<usize> {
        if self.via_linked || self.when_vein_empty {
            return Vec::new();
        }
        game.wos.get(&wo).and_then(|w| w.linked_groups.clone()).unwrap_or_default()
    }
}

const RAY_CHECK_EVERY: f32 = 5.0;

pub struct Rays {
    next: f32,
    /// _miningRaysCasted.
    cast: bool,
    /// _miningTerraStage: TerraformStagesHandler.GetCurrentGlobalStage at the last cast.
    stage: Option<String>,
    /// updateMiningRaysWhenChangingTerraStage.
    update: bool,
    /// The 5 s wait before a cast.
    pending: bool,
}

/// TryToCastRay, every 5 s while the machine is active (on the current planet): the first cast
/// (5 s later while the world still loads), then with updateMiningRaysWhenChangingTerraStage a
/// cast 5 s after each new global stage it sees. A cast resets the linked groups to the veins
/// found and adds those to groupDatas once more.
fn mining_rays(game: &mut Game, wo: i32, now: f32) {
    let stage = crate::meteo::current_global_stage(game);
    let (active, loaded) = (is_active(game, wo), game.ext.machines.loaded);
    let g = game.ext.machines.get_mut(&wo).unwrap().generator.as_mut().unwrap();
    let r = g.rays.as_mut().unwrap();
    r.next = now + RAY_CHECK_EVERY;
    let cast = if r.pending {
        r.pending = false;
        // A travel during the wait: the game casts into the new planet's scene; this one waits
        // until the machine is back.
        active
    } else if active && (!r.cast || (r.update && r.stage != stage)) {
        r.pending = r.cast || !loaded;
        !r.pending
    } else {
        false
    };
    if cast {
        (r.cast, r.stage) = (true, stage);
        if !r.update {
            g.rays = None;
        }
        if let Some(f) = game.wos.get(&wo).and_then(|w| super::root(game, w.group, "MachineGenerator")) {
            super::veins::cast(game, wo, &f);
        }
        let g = game.ext.machines.list.get(&wo).unwrap().generator.as_ref().unwrap();
        let veins = g.vein_groups(game, wo);
        game.ext.machines.get_mut(&wo).unwrap().generator.as_mut().unwrap().groups.extend(veins);
    }
}

/// AddToGenerationSpeedMultiplier: the loop restarts with the new period.
fn add_generation_speed(game: &mut Game, wo: i32, add: f32) {
    let (now, speed) = (game.time, game.rules.speed.machines);
    if let Some(g) = game.ext.machines.get_mut(&wo).and_then(|m| m.generator.as_mut()) {
        g.speed += add;
        g.next = now + g.period() / speed;
    }
}

/// TryToGenerate, one round.
fn try_to_generate(game: &mut Game, wo: i32) {
    let inventory = game.ext.machines.list[&wo].linked;
    if !has_energy(game, wo) || is_full(game, inventory) {
        return;
    }
    let vein = game.wos.get(&wo).map(|w| w.linked_wo).unwrap_or(0);
    if vein == 0 {
        generate_an_object(game, wo, inventory);
        return;
    }
    let Some(count) = game.wos.get(&vein).map(|v| v.count) else {
        game.ext.machines.get_mut(&wo).unwrap().generator.as_mut().unwrap().dead = true;
        return;
    };
    let when_empty = game.ext.machines.list[&wo].generator.as_ref().unwrap().when_vein_empty;
    if !when_empty && count[0] < count[1] {
        set_scene_growth_and_count(game, vein, [count[0] + 1, count[1]]);
        generate_an_object(game, wo, inventory);
    } else if when_empty && count[0] == count[1] {
        generate_an_object(game, wo, inventory);
    }
}

/// WorldObjectsHandler.SetSceneWorldObjectGrowthAndCount (SetSceneWorldObjectGrowthServerRpc).
fn set_scene_growth_and_count(game: &mut Game, vein: i32, count: [i32; 2]) {
    let Some(w) = game.wos.get_mut(&vein) else { return };
    w.count = count;
    w.growth = count[0] as f32 / count[1] as f32 * 100.0;
    let hash = game.data.groups[w.group].hash;
    game.net.rpc_scene(Target::All, "WorldObjectsHandler.SetSceneWorldObjectGrowthClientRpc", &[json!(vein), json!(hash), json!(count)]);
}

/// GenerateAnObject: one of the machine's groups into its inventory.
fn generate_an_object(game: &mut Game, wo: i32, inventory: i32) {
    let g = game.ext.machines.list[&wo].generator.as_ref().unwrap();
    let (via_linked, mut list, terra, stage) = (g.via_linked, g.groups.clone(), g.terra_groups.clone(), g.terra_stage.clone());
    let linked = game.wos.get(&wo).and_then(|w| w.linked_groups.clone());
    let group = if via_linked {
        match linked {
            Some(l) if !l.is_empty() => Some(l[game.rng.range(0, l.len() as i32) as usize]),
            _ => None,
        }
    } else if !list.is_empty() || !terra.is_empty() {
        list.extend(linked.unwrap_or_default());
        if !terra.is_empty() && reached_stage(game, wo, &stage) {
            list.extend(terra);
        }
        (!list.is_empty()).then(|| list[game.rng.range(0, list.len() as i32) as usize])
    } else {
        None
    };
    if let Some(group) = group {
        add_new(game, group, inventory);
    }
}

/// IsWorldValuesAreBetweenStages(stage of the current planet, null, the machine's planet).
fn reached_stage(game: &Game, wo: i32, field: &str) -> bool {
    let planet = game.planet_id();
    let Some(stage) = game.data.asset(&format!("PlanetData:PlanetData-{planet}")).and_then(|p| p[field].as_str()).and_then(|s| game.data.asset(s)) else { return true };
    let Some(kind) = stage["unitType"].as_str().and_then(crate::units::Kind::from_name) else { return false };
    let value = crate::units::value(game, kind, game.wos.get(&wo).map(|w| w.planet_hash()).unwrap_or(0));
    value < 0.0 || value >= stage["startValue"].as_f64().unwrap_or(0.0)
}

// ---- MachineAutoCrafter -------------------------------------------------------------------

pub struct AutoCrafter {
    pub next: f32,
    pub period: f32,
    range: f32,
}

impl AutoCrafter {
    /// Awake: TryToCraft waits (instance count % 50) / 10 s before its loop.
    pub fn new(f: &Value, now: f32, instanced: i32) -> AutoCrafter {
        AutoCrafter { next: now + (instanced % 50) as f32 / 10.0, period: f32_of(f, "craftEveryXSec"), range: f32_of(f, "range") }
    }
}

fn try_to_craft(game: &mut Game, wo: i32) {
    if !has_energy(game, wo) {
        return;
    }
    let inventory = game.ext.machines.list[&wo].linked;
    let range = game.ext.machines.list[&wo].autocrafter.as_ref().unwrap().range;
    if inventory == 0 || is_full(game, inventory) {
        return;
    }
    let Some(target) = game.wos.get(&wo).and_then(|w| w.linked_groups.as_ref()).and_then(|l| l.first().copied()) else { return };
    let available = items_in_range(game, wo, range);
    craft_if_possible(game, wo, inventory, target, &available);
}

/// SetItemsInRange: loose items (not growing) and what logistics-enabled buildings hold.
fn items_in_range(game: &Game, wo: i32, range: f32) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    let add = |id: i32, out: &mut Vec<i32>| {
        if !out.contains(&id) {
            out.push(id);
        }
    };
    for id in in_range(game, wo, range) {
        let w = &game.wos[&id];
        let g = &game.data.groups[w.group];
        if !g.constructible {
            if !(w.growth > 0.0 && w.growth < 100.0) {
                add(id, &mut out);
            }
            continue;
        }
        let logistic = g.str("logisticInterplanetaryType");
        let associated = !components(game, w.group, "InventoryAssociated", false).is_empty() || w.linked_inventory != 0;
        let proxies = components(game, w.group, "InventoryAssociatedProxy", false);
        if logistic == "Disabled" || (!associated && proxies.is_empty()) {
            continue;
        }
        for p in proxies {
            let index = p["_secondaryInventoryIndex"].as_i64().unwrap_or(-1);
            if logistic == "EnabledOnSecondaryInventories" && index == -1 {
                continue;
            }
            let inventory = if index < 0 { w.linked_inventory } else { w.secondary_inventories.get(index as usize).copied().unwrap_or(0) };
            for item in items(game, inventory) {
                if game.wos.get(&item).is_some_and(|i| !i.is_placed() || i.growth == 100.0) {
                    add(item, &mut out);
                }
            }
        }
    }
    out
}

/// CraftIfPossible: the recipe's ingredients from what is in range make one item.
fn craft_if_possible(game: &mut Game, wo: i32, inventory: i32, target: usize, available: &[i32]) {
    let mut recipe = game.data.groups[target].recipe.clone();
    let mut used = Vec::new();
    for id in available {
        let group = game.wos[id].group;
        if let Some(at) = recipe.iter().position(|g| *g == group) {
            recipe.remove(at);
            used.push(*id);
            if recipe.is_empty() {
                break;
            }
        }
    }
    if !recipe.is_empty() {
        return;
    }
    game.destroy_world_objects(&used, true);
    add_new(game, target, inventory);
    if is_active(game, wo) {
        // ActionCrafter.CraftAnimation -> StartCraftAnimationServerRpc from the server.
        if let Some(net) = game.wos.get(&wo).and_then(|w| w.net) {
            let hash = game.data.groups[target].hash;
            game.net.rpc(Target::All, net, "ConstructibleProxy.StartCraftAnimationClientRpc", &[json!(hash)]);
        }
    }
    game.world_objects_rpc(0, "WorldObjectsHandler.AddOneToTotalCraftServerRpc", &crate::game::Args(Vec::new()));
}

// ---- MachineDisintegrator -----------------------------------------------------------------

pub struct Disintegrator {
    pub next: f32,
    pub period: f32,
    give: i32,
}

impl Disintegrator {
    /// SetDisintegratorInventory: the loop starts once the second inventory is known (the frame
    /// after the spawn), then breaks an object every breakEveryXSec.
    pub fn new(f: &Value, now: f32) -> Disintegrator {
        let period = f["breakEveryXSec"].as_f64().unwrap_or(0.0) as f32;
        Disintegrator { next: now + period, period, give: f["giveXIngredientsBack"].as_i64().unwrap_or(0) as i32 }
    }
}

/// TryToDesintegrateAnObjectInInventory: the first item whose ingredients fit goes, and up to
/// giveXIngredientsBack of them (picked at random) come out.
fn try_to_disintegrate(game: &mut Game, wo: i32) {
    let first = game.ext.machines.list[&wo].linked;
    let give = game.ext.machines.list[&wo].disintegrator.as_ref().unwrap().give;
    let Some(second) = game.wos.get(&wo).and_then(|w| w.secondary_inventories.first().copied()) else { return };
    let Some(inv) = game.inventories.get(&second) else { return };
    if inv.is_full() {
        return;
    }
    let free = inv.size - inv.items.len() as i32;
    for item in items(game, first) {
        let Some(group) = game.wos.get(&item).map(|w| w.group) else { continue };
        let recipe = game.data.groups[group].recipe.clone();
        if recipe.is_empty() || free < give.min(recipe.len() as i32) {
            continue;
        }
        let mut list = recipe;
        remove_item(game, item, first, true);
        for _ in 0..give {
            let at = game.rng.range(0, list.len() as i32) as usize;
            add_new(game, list[at], second);
            list.remove(at);
            if list.is_empty() {
                break;
            }
        }
        break;
    }
}

// ---- MachineOptimizer ---------------------------------------------------------------------

pub struct Optimizer {
    range: f32,
    max: usize,
    /// _modifiedWorldObjects with _fuseGroupsForWorldObjects.
    modified: Vec<(i32, usize)>,
}

impl Optimizer {
    pub fn new(f: &Value) -> Optimizer {
        Optimizer { range: f32_of(f, "range"), max: f["maxWorldObjectPerFuse"].as_i64().unwrap_or(0).max(0) as usize, modified: Vec::new() }
    }
}

/// _fuseCategoryToWorldUnit.
fn fuse_unit(sub_category: &str) -> Option<&'static str> {
    Some(match sub_category {
        "FusePressure" => "Pressure",
        "FuseOxygen" => "Oxygen",
        "FuseHeat" => "Heat",
        "FusePlants" => "Plants",
        "FuseEnergy" => "Energy",
        "FuseAnimals" => "Animals",
        "FuseInsects" => "Insects",
        "FusePurification" => "Purification",
        _ => return None,
    })
}

fn fuse_value(game: &Game, fuse: usize) -> f32 {
    game.data.groups[fuse].fields["value"].as_i64().unwrap_or(0) as f32 / 100.0
}

/// MachineGrowerBase.AddToGrowthSpeedMultiplier on each grower component of a machine.
pub(super) fn add_growth_speed(game: &mut Game, wo: i32, add: f32) {
    let Some(m) = game.ext.machines.get_mut(&wo) else { return };
    // Every component keeps its own multiplier, all the same.
    let count = m.growers.len();
    m.grow_multiplier += add;
    let Some(net) = game.wos.get(&wo).and_then(|w| w.net) else { return };
    for _ in 0..count {
        game.net.rpc(Target::All, net, "ConstructibleProxy.SetGrowerSpeedMultiplierClientRpc", &[json!(add)]);
    }
}

/// ChangeMultiplierDependingOnFuses, server side.
pub fn change_multiplier(game: &mut Game, wo: i32) {
    remove_previously_set(game, wo);
    let Some(o) = game.ext.machines.list.get(&wo).and_then(|m| m.optimizer.as_ref()) else { return };
    let (range, max) = (o.range, o.max);
    let inventory = game.ext.machines.list[&wo].linked;
    let planet = game.wos.get(&wo).map(|w| w.planet_hash()).unwrap_or(0);
    let mut modified = Vec::new();
    for fuse_item in items(game, inventory) {
        let Some(fuse) = game.wos.get(&fuse_item).map(|w| w.group) else { continue };
        let sub = game.data.groups[fuse].str("itemSubCategory").to_string();
        if let Some(unit) = fuse_unit(&sub) {
            let index = crate::model::unit_index(unit).unwrap();
            let machines: Vec<i32> = in_range(game, wo, range).into_iter().filter(|id| {
                let g = &game.data.groups[game.wos[id].group];
                g.constructible && g.units[index] > 0.0
            }).collect();
            let add = game.data.groups[fuse].units[index] / 100.0;
            for id in by_distance(game, wo, machines).into_iter().take(max) {
                game.wos.get_mut(&id).unwrap().unit_multiplier[index] += add;
                modified.push((id, fuse));
            }
            force_reset(game, unit, planet);
        } else if sub == "FuseProduction" {
            let machines: Vec<i32> = in_range(game, wo, range).into_iter().filter(|id| !components(game, game.wos[id].group, "MachineGenerator", true).is_empty()).collect();
            for id in by_distance(game, wo, machines).into_iter().take(max) {
                add_generation_speed(game, id, fuse_value(game, fuse));
                modified.push((id, fuse));
            }
        } else if sub == "FuseTradeRockets" {
            let machines: Vec<i32> = in_range(game, wo, range).into_iter().filter(|id| super::is_rocket_platform(game, game.wos[id].group)).collect();
            for id in by_distance(game, wo, machines).into_iter().take(max) {
                crate::rockets::add_speed_multiplier(game, id, fuse_value(game, fuse));
                modified.push((id, fuse));
            }
        } else if sub == "FuseGrowth" {
            let machines: Vec<i32> = in_range(game, wo, range).into_iter().filter(|id| game.ext.machines.list.get(id).is_some_and(|m| m.growers.contains(&true))).collect();
            for id in by_distance(game, wo, machines).into_iter().take(max) {
                add_growth_speed(game, id, fuse_value(game, fuse));
                modified.push((id, fuse));
            }
        }
    }
    if let Some(o) = game.ext.machines.get_mut(&wo).and_then(|m| m.optimizer.as_mut()) {
        o.modified = modified;
    }
    // RequireEnergyHandler.UpdateAllEnergyRequesterDelayed.
    crate::energy::update_all_energy_requesters(game);
}

/// RemovePreviouslySetWorldObjects: every multiplier the fuses gave is taken back.
fn remove_previously_set(game: &mut Game, wo: i32) {
    let Some(o) = game.ext.machines.get_mut(&wo).and_then(|m| m.optimizer.as_mut()) else { return };
    let modified = std::mem::take(&mut o.modified);
    let planet = game.wos.get(&wo).map(|w| w.planet_hash()).unwrap_or(0);
    for (id, fuse) in modified {
        let sub = game.data.groups[fuse].str("itemSubCategory").to_string();
        if let Some(unit) = fuse_unit(&sub) {
            let index = crate::model::unit_index(unit).unwrap();
            let add = game.data.groups[fuse].units[index] / 100.0 * -1.0;
            if let Some(w) = game.wos.get_mut(&id) {
                w.unit_multiplier[index] += add;
            }
            force_reset(game, unit, planet);
        } else if game.wos.get(&id).is_some_and(|w| w.net.is_some()) {
            let add = fuse_value(game, fuse) * -1.0;
            match sub.as_str() {
                "FuseProduction" => add_generation_speed(game, id, add),
                "FuseTradeRockets" => crate::rockets::add_speed_multiplier(game, id, add),
                "FuseGrowth" => add_growth_speed(game, id, add),
                _ => {}
            }
        }
    }
}

/// ConstructibleProxy.MachineOptimizerUpdate: clients redo their own view of the fuses.
pub fn optimizer_update(game: &mut Game, wo: i32) {
    if let Some(net) = game.wos.get(&wo).and_then(|w| w.net) {
        game.net.rpc(Target::All, net, "ConstructibleProxy.MachineOptimizerUpdateClientRpc", &[]);
    }
}

/// MachineOptimizer.OnConstructibleInstantiated: a producing building came in range.
pub fn constructible_instantiated(game: &mut Game, wo: i32, position: [f32; 3]) {
    let Some(w) = game.wos.get(&wo) else { return };
    if position == [0.0; 3] || !w.has_unit_generation(&game.data) {
        return;
    }
    let optimizers: Vec<i32> = game.ext.machines.list.iter().filter(|(_, m)| m.optimizer.is_some()).map(|(k, _)| *k).collect();
    for o in optimizers {
        let range = game.ext.machines.list[&o].optimizer.as_ref().unwrap().range;
        if game.wos.get(&o).is_some_and(|me| dist(position, me.position) <= range) {
            change_multiplier(game, o);
            optimizer_update(game, o);
        }
    }
}

/// MachineOptimizer.OnConstructibleDestroyed: a building a fuse was boosting went.
pub fn constructible_destroyed(game: &mut Game, wo: i32) {
    let optimizers: Vec<i32> = game.ext.machines.list.iter().filter(|(_, m)| m.optimizer.as_ref().is_some_and(|o| o.modified.iter().any(|(id, _)| *id == wo))).map(|(k, _)| *k).collect();
    for o in optimizers {
        change_multiplier(game, o);
        optimizer_update(game, o);
    }
}

// ---- MachineDestructInventoryIfFull ---------------------------------------------------------

/// TryToCleanInventory: set to destroy (setting 1) and full, everything inside goes.
pub fn try_to_clean(game: &mut Game, wo: i32) {
    let inventory = game.ext.machines.list[&wo].linked;
    if game.wos.get(&wo).is_some_and(|w| w.setting == 1) && is_full(game, inventory) && game.inventories.contains_key(&inventory) {
        super::remove_and_destroy_all(game, inventory);
        // The destructor's ActionnableInteractive has no proxy: its animation stays local.
    }
}

// ---- ConstructibleProxy.RecycleContentServerRpc ----------------------------------------------

/// The recycler's first item: a container's content drops out, an unrecyclable item drops,
/// anything else breaks into its ingredients, dropped at random in the bounds.
pub fn recycle_content(game: &mut Game, wo: i32, center: [f32; 3], size: [f32; 3]) {
    let Some(inventory) = game.wos.get(&wo).map(|w| w.linked_inventory) else { return };
    let Some(&item) = game.inventories.get(&inventory).and_then(|i| i.items.first()) else { return };
    let Some(w) = game.wos.get(&item) else { return };
    let (group, content) = (w.group, w.linked_inventory);
    let recipe = game.data.groups[group].recipe.clone();
    let drop = |game: &mut Game, id: i32| game.drop_on_floor(id, center, [0.0, 0.0, 0.0, 1.0], 0.0, true, true, 0, 0.0, false, 0);
    if content > 0 && game.inventories.get(&content).is_some_and(|i| !i.items.is_empty()) {
        if remove_item(game, item, inventory, false) {
            for inside in items(game, content).into_iter().rev() {
                remove_item(game, inside, content, false);
                drop(game, inside);
            }
            game.destroy_world_object(item, false);
        }
        return;
    }
    if recipe.is_empty() || game.data.groups[group].bool("cantBeRecycled") {
        if remove_item(game, item, inventory, false) {
            drop(game, item);
        }
        return;
    }
    if remove_item(game, item, inventory, true) {
        let (min, max) = ([0, 1, 2].map(|i| center[i] - size[i] / 2.0), [0, 1, 2].map(|i| center[i] + size[i] / 2.0));
        for ingredient in recipe {
            let position = [0, 1, 2].map(|i| game.rng.rangef(min[i], max[i]));
            game.create_and_drop_on_floor(ingredient, position, 0.6, true, true, 0.0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::tests::{build, game};

    #[test]
    fn generation_speed_rounds_halves_to_even() {
        let g = |spawn: f32, speed: f32| Generator { next: 0.0, spawn_every: spawn, speed, groups: vec![], terra_groups: vec![], terra_stage: String::new(), via_linked: false, when_vein_empty: false, rays: None, dead: false }.period();
        assert_eq!(g(70.0, 1.0), 70.0);
        assert_eq!(g(65.0, 1.25), 52.0);
        assert_eq!(g(5.0, 2.0), 2.0);
        assert_eq!(g(15.0, 2.0), 8.0);
    }

    #[test]
    fn generators_fill_their_inventory_when_powered() {
        let Some(mut game) = game() else { return };
        let collector = build(&mut game, "WaterCollector1", [0.0, 5.0, 0.0]);
        let inv = game.wos[&collector].linked_inventory;
        game.time += 150.0;
        crate::machines::tick(&mut game);
        assert!(game.inventories[&inv].items.is_empty(), "no energy");
        game.ext.machines.energy.insert(collector, true);
        game.time += 150.0;
        crate::machines::tick(&mut game);
        let bottle = game.data.group("WaterBottle1").unwrap();
        assert_eq!(game.inventories[&inv].items.iter().map(|i| game.wos[i].group).collect::<Vec<_>>(), vec![bottle]);
    }

    #[test]
    fn mining_rays_are_cast_again_at_each_terraform_stage() {
        let Some(mut game) = game() else { return };
        // The saved linked groups stand for the veins.
        crate::machines::veins::without_veins(&mut game);
        let g = game.data.group("WaterLifeCollector1").unwrap();
        let id = game.new_world_object_id();
        game.create_world_object(g, id, true);
        let fish = game.data.group("Fish1Eggs").or(game.data.group("Iron")).unwrap();
        let planet = game.planet_hash();
        let w = game.wos.get_mut(&id).unwrap();
        (w.position, w.rotation, w.planet, w.linked_groups) = ([0.0, 5.0, 0.0], [0.0, 0.0, 0.0, 1.0], planet, Some(vec![fish]));
        game.instantiate(id, false, 0);
        let groups = |game: &Game| game.ext.machines.list[&id].generator.as_ref().unwrap().groups.iter().filter(|g| **g == fish).count();
        assert_eq!(groups(&game), 1);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(groups(&game), 1, "same stage, no new cast");
        let p = game.units.planet_mut(planet).unwrap();
        p.units.iter_mut().filter(|u| u.kind == crate::units::Kind::Terraformation).for_each(|u| u.total = 1.0e12);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(groups(&game), 1, "the cast comes 5 s after the change is seen");
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(groups(&game), 2);
        assert_eq!(game.wos[&id].linked_groups, Some(vec![fish]));
    }

    /// A generator placed on `planet` with saved linked groups, as the save loads it.
    fn placed(game: &mut Game, group: &str, position: [f32; 3], planet: i32, linked: Vec<usize>) -> i32 {
        let g = game.data.group(group).unwrap();
        let id = game.new_world_object_id();
        game.create_world_object(g, id, true);
        let w = game.wos.get_mut(&id).unwrap();
        (w.position, w.rotation, w.planet, w.linked_groups) = (position, [0.0, 0.0, 0.0, 1.0], planet, Some(linked));
        game.instantiate(id, false, 0);
        id
    }

    fn count(game: &Game, wo: i32, group: usize) -> usize {
        game.ext.machines.list[&wo].generator.as_ref().unwrap().groups.iter().filter(|g| **g == group).count()
    }

    #[test]
    fn extractors_on_other_planets_cast_once_they_are_there() {
        let Some(mut game) = game() else { return };
        let (prime, prime_hash) = (game.planet, game.planet_hash());
        game.planet = game.net_data.planet_index("Aqualis").unwrap();
        let (iron, iridium) = (game.data.group("Iron").unwrap(), game.data.group("Iridium").unwrap());
        // Over Prime's tier-1 Iridium vein, loaded while the server is on Aqualis.
        let id = placed(&mut game, "OreExtractor1", [2083.0, 30.0, -76.0], prime_hash, vec![iron]);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&id].linked_groups, Some(vec![iron]), "kept until the machine is active");
        assert_eq!(count(&game, id, iridium), 0);
        game.planet = prime;
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&id].linked_groups, Some(vec![iridium, iridium]));
        assert_eq!(count(&game, id, iridium), 2);
        assert!(game.ext.machines.list[&id].generator.as_ref().unwrap().rays.is_none(), "cast once for good");
    }

    #[test]
    fn mining_rays_find_veins_that_show_later() {
        let Some(mut game) = game() else { return };
        let planet = game.planet_hash();
        let fish = game.data.group("Fish1Eggs").unwrap();
        // Prime's Fish1Eggs vein at (501, -281, 436) is hidden before the Fish stage.
        let id = placed(&mut game, "WaterLifeCollector1", [501.0, 30.0, 436.0], planet, vec![]);
        assert_eq!(game.wos[&id].linked_groups, Some(vec![]));
        let p = game.units.planet_mut(planet).unwrap();
        p.units.iter_mut().filter(|u| u.kind == crate::units::Kind::Terraformation).for_each(|u| u.total = 1.0e12);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        game.time += 5.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&id].linked_groups, Some(vec![fish]));
        assert_eq!(count(&game, id, fish), 1);
    }

    #[test]
    fn extractors_count_down_their_vein() {
        let Some(mut game) = game() else { return };
        let extractor = build(&mut game, "OreExtractor1", [0.0, 5.0, 0.0]);
        let vein_group = game.data.group("GenerationGroupVein").unwrap();
        let vein = game.create_world_object(vein_group, 100_000_001, true);
        game.wos.get_mut(&vein).unwrap().count = [9, 10];
        game.wos.get_mut(&extractor).unwrap().linked_wo = vein;
        game.ext.machines.energy.insert(extractor, true);
        game.time += 70.0;
        crate::machines::tick(&mut game);
        assert_eq!(game.wos[&vein].count, [10, 10]);
        assert_eq!(game.wos[&vein].growth, 100.0);
        game.time += 70.0;
        crate::machines::tick(&mut game);
        let inv = game.wos[&extractor].linked_inventory;
        assert_eq!(game.inventories[&inv].items.len(), 1, "an empty vein gives nothing more");
    }

    #[test]
    fn autocrafters_use_items_in_range() {
        let Some(mut game) = game() else { return };
        let crafter = build(&mut game, "AutoCrafter1", [0.0, 5.0, 0.0]);
        game.ext.machines.energy.insert(crafter, true);
        let chest = build(&mut game, "Container1", [5.0, 5.0, 0.0]);
        let target = game.data.group("Rod-iridium").unwrap();
        game.wos.get_mut(&crafter).unwrap().linked_groups = Some(vec![target]);
        let chest_inv = game.wos[&chest].linked_inventory;
        for g in game.data.groups[target].recipe.clone() {
            game.add_new_item(g, chest_inv, 0);
        }
        // Containers share all their inventories with logistics, so crafters use what they hold.
        assert_eq!(game.data.groups[game.wos[&chest].group].str("logisticInterplanetaryType"), "EnabledOnAllInventories");
        game.time += 10.0;
        crate::machines::tick(&mut game);
        let inv = game.wos[&crafter].linked_inventory;
        assert_eq!(game.inventories[&inv].items.iter().map(|i| game.wos[i].group).collect::<Vec<_>>(), vec![target]);
        assert!(game.inventories[&chest_inv].items.is_empty());
    }

    #[test]
    fn disintegrators_break_items_into_ingredients() {
        let Some(mut game) = game() else { return };
        let breaker = build(&mut game, "OreBreaker1", [0.0, 5.0, 0.0]);
        game.ext.machines.energy.insert(breaker, true);
        let inv = game.wos[&breaker].linked_inventory;
        let rod = game.data.group("Rod-iridium").unwrap();
        game.add_new_item(rod, inv, 0);
        game.time += 130.0;
        crate::machines::tick(&mut game);
        let second = game.wos[&breaker].secondary_inventories[0];
        let recipe = &game.data.groups[rod].recipe;
        assert!(game.inventories[&inv].items.is_empty());
        assert_eq!(game.inventories[&second].items.len(), recipe.len().min(3));
    }

    #[test]
    fn recyclers_break_items_and_drop_what_cannot_be() {
        let Some(mut game) = game() else { return };
        let recycler = build(&mut game, "RecyclingMachine", [0.0, 5.0, 0.0]);
        let inv = game.wos[&recycler].linked_inventory;
        let backpack = game.data.group("Backpack1").unwrap();
        game.add_new_item(backpack, inv, 0);
        let before = game.wos.len();
        recycle_content(&mut game, recycler, [0.0, 6.0, 0.0], [1.0, 1.0, 1.0]);
        assert!(game.inventories[&inv].items.is_empty());
        let iron = game.data.group("Iron").unwrap();
        let dropped: Vec<&crate::model::WorldObject> = game.wos.values().filter(|w| w.group == iron && w.is_placed()).collect();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|w| (-0.5..=0.5).contains(&w.position[0]) && (5.5..=6.5).contains(&w.position[1])));
        assert_eq!(game.wos.len(), before + 1, "the backpack went, its two ingredients came");
        // No recipe: the item itself drops.
        game.add_new_item(iron, inv, 0);
        let item = game.inventories[&inv].items[0];
        recycle_content(&mut game, recycler, [0.0, 6.0, 0.0], [1.0, 1.0, 1.0]);
        assert_eq!(game.wos[&item].position, [0.0, 6.0, 0.0]);
    }

    #[test]
    fn destructors_empty_themselves_when_full_and_set_to() {
        let Some(mut game) = game() else { return };
        let destructor = build(&mut game, "Destructor1", [0.0, 5.0, 0.0]);
        let inv = game.wos[&destructor].linked_inventory;
        let iron = game.data.group("Iron").unwrap();
        for _ in 0..30 {
            game.add_new_item(iron, inv, 0);
        }
        assert!(game.inventories[&inv].is_full(), "setting 0 keeps everything");
        game.wos.get_mut(&destructor).unwrap().setting = 1;
        let item = game.inventories[&inv].items[0];
        game.inventories_rpc(1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &crate::game::Args(vec![json!(item), json!(inv), json!(true)]));
        game.add_new_item(iron, inv, 0);
        assert!(game.inventories[&inv].items.is_empty());
    }

    #[test]
    fn optimizers_boost_and_release_machines_in_range() {
        let Some(mut game) = game() else { return };
        let optimizer = build(&mut game, "Optimizer1", [0.0, 5.0, 0.0]);
        let near = build(&mut game, "Vegetube1", [10.0, 5.0, 0.0]);
        let far = build(&mut game, "Vegetube1", [500.0, 5.0, 0.0]);
        let inv = game.wos[&optimizer].linked_inventory;
        let oxygen = crate::model::unit_index("Oxygen").unwrap();
        let fuse = game.data.group("FuseOxygen1").unwrap();
        game.add_new_item(fuse, inv, 0);
        assert_eq!(game.wos[&near].unit_multiplier[oxygen], 5.0);
        assert_eq!(game.wos[&far].unit_multiplier[oxygen], 0.0);
        let item = game.inventories[&inv].items[0];
        game.inventories_rpc(1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &crate::game::Args(vec![json!(item), json!(inv), json!(true)]));
        assert_eq!(game.wos[&near].unit_multiplier[oxygen], 0.0);
        // Growth fuses speed up growers (one ClientRpc per grower component).
        game.add_new_item(game.data.group("FuseGrowth1").unwrap(), inv, 0);
        assert!(game.ext.machines.list[&near].grow_multiplier > 1.0);
        // Trade rocket fuses go to the rockets' platforms.
        build(&mut game, "TradePlatform1", [20.0, 5.0, 0.0]);
        let item = game.inventories[&inv].items[0];
        game.inventories_rpc(1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &crate::game::Args(vec![json!(item), json!(inv), json!(true)]));
        assert_eq!(game.ext.machines.list[&near].grow_multiplier, 1.0);
        game.net.out.clear();
        game.add_new_item(game.data.group("FuseTradeRocketsSpeed1").unwrap(), inv, 0);
        let sent = crate::machines::tests::sent(&game);
        assert!(sent.iter().any(|(name, _)| name == "ConstructibleProxy.SetRocketBackAndForthSpeedMultiplierClientRpc"), "{sent:?}");
    }
}
