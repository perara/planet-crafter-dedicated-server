//! SpaceCraft.WorldObjectsHandler, server side: creating, placing, dropping and destroying
//! world objects, the group counts every client shows, and the scene objects players removed.

use serde_json::json;

use crate::game::{Args, Game};
use crate::model::{is_from_scene, WorldObject};
use crate::net::{enc, ListEvent, NetObj, Target, VarValue};

const HANDLER: &str = "WorldObjectsHandler";
const DONT_DESTROY_WITH_OWNER: u16 = 1 << 7;

impl Game {
    /// WorldObjectsIdHandler.GetNewWorldObjectIdForDb.
    pub fn new_world_object_id(&mut self) -> i32 {
        for _ in 0..10_000 {
            let id = 20 * 10_000_000 + self.rng.range(1_000_000, 9_999_999);
            if !self.wos.contains_key(&id) {
                return id;
            }
        }
        0
    }

    /// CreateNewWorldObject: stores a new world object (id 0 picks a fresh id); returns its id.
    pub fn create_world_object(&mut self, group: usize, id: i32, save: bool) -> i32 {
        let fresh = id == 0;
        let id = if fresh { self.new_world_object_id() } else { id };
        let mut wo = WorldObject::new(id, group, &self.data);
        wo.dont_save = !save;
        if fresh && self.data.groups[group].id == crate::genetics::TRAIT_GROUP {
            crate::genetics::randomize_trait(self, &mut wo);
        }
        self.wos.insert(id, wo);
        self.add_to_specific_lists(id);
        self.change_state_index(id);
        id
    }

    /// ChangeStateIndex: placed buildings changed, so the units recount their rates.
    pub fn change_state_index(&mut self, id: i32) {
        if self.wos.get(&id).is_some_and(|w| self.data.groups[w.group].constructible) && !is_from_scene(id) {
            crate::units::constructed_changed(self);
        }
    }

    fn is_constructed_kind(&self, group: usize) -> bool {
        let g = &self.data.groups[group];
        g.constructible || g.usable_type() == "Buildable"
    }

    /// AddToSpecificLists: the group counts and the constructed / drone-pickable sets.
    pub fn add_to_specific_lists(&mut self, id: i32) {
        let Some(wo) = self.wos.get(&id) else { return };
        let (group, dont_save) = (wo.group, wo.dont_save);
        let hash = self.data.groups[group].hash;
        if !dont_save {
            self.count_list(true, "_allWorldObjectsGroups", hash);
        }
        if self.is_constructed_kind(group) {
            if !is_from_scene(id) {
                self.constructed.insert(id);
                self.count_list(true, "_constructedWorldObjectsGroups", hash);
            }
        } else {
            let g = &self.data.groups[group];
            if !g.constructible && g.bool("canBePickedUpFromWorldByDrones") && g.str("displayInLogisticType") == "Display" {
                self.pickables.insert(id);
            }
        }
    }

    /// UpdateWorldObjectSaveState.
    pub fn update_save_state(&mut self, id: i32) {
        self.add_to_specific_lists(id);
    }

    /// Increment/DecrementNetworkList on a GroupCountData list of the handler.
    fn count_list(&mut self, increment: bool, var: &str, hash: i32) {
        let Some((object, _)) = self.net.find_class(HANDLER) else { return };
        let items = match self.net.get(object, HANDLER, var) {
            Some(VarValue::List(items)) => items.clone(),
            _ => return,
        };
        let index = items.iter().position(|i| i32::from_le_bytes(i[0..4].try_into().unwrap()) == hash);
        let event = match (index, increment) {
            (None, true) => ListEvent::Add(Game::count_item(hash, 1)),
            (None, false) => return,
            (Some(i), _) => {
                let count = i32::from_le_bytes(items[i][4..8].try_into().unwrap()) + if increment { 1 } else { -1 };
                if count == 0 { ListEvent::RemoveAt(i as i32) } else { ListEvent::Value(i as i32, Game::count_item(hash, count)) }
            }
        };
        self.net.list_events(object, HANDLER, var, vec![event]);
    }

    /// Count of a group in one of the handler's lists.
    pub fn group_count(&self, constructed: bool, group: usize) -> i32 {
        let var = if constructed { "_constructedWorldObjectsGroups" } else { "_allWorldObjectsGroups" };
        let hash = self.data.groups[group].hash;
        let Some((object, _)) = self.net.find_class(HANDLER) else { return 0 };
        match self.net.get(object, HANDLER, var) {
            Some(VarValue::List(items)) => items.iter().find(|i| i32::from_le_bytes(i[0..4].try_into().unwrap()) == hash).map(|i| i32::from_le_bytes(i[4..8].try_into().unwrap())).unwrap_or(0),
            _ => 0,
        }
    }

    pub fn first_world_object_of_group(&self, group: usize, planet: i32) -> Option<i32> {
        self.wos.values().find(|w| w.group == group && (planet == 0 || w.planet_hash() == planet)).map(|w| w.id)
    }

    /// InstantiateWorldObject: the world object's network object, with its variables from
    /// the world object, its inventories, and its spawn for everyone.
    pub fn instantiate(&mut self, id: i32, from_db: bool, owner: u64) -> Option<u64> {
        let wo = self.wos.get_mut(&id)?;
        wo.from_db = from_db;
        let group = wo.group;
        let g = &self.data.groups[group];
        if !g.has_prefab {
            return None;
        }
        let own_prefab = self.data.layouts.get(&g.id).is_some_and(|l| !l.prefab.starts_with("GroupNetworkContainer"));
        if own_prefab && is_from_scene(id) {
            return None;
        }
        let key = if !own_prefab && g.inventory_size <= 0 && from_db { format!("{}#static", g.id) } else { g.id.clone() };
        let Some(layout) = self.data.layouts.get(&key) else {
            eprintln!("no network layout for {key}");
            return None;
        };
        let mut o = NetObj::from_layout(layout);
        let (inventory_size, secondary, hash) = (g.inventory_size, g.secondary_inventories.clone(), g.hash);
        let constructible = g.constructible;
        let energy = g.unit("Energy");
        if inventory_size > 0 && self.wos[&id].linked_inventory == 0 {
            self.create_inventory(inventory_size, 0, id, &[], &[]);
        }
        if !secondary.is_empty() && self.wos[&id].secondary_inventories.is_empty() {
            for size in secondary {
                let inv = self.create_inventory(size, 0, 0, &[], &[]);
                self.wos.get_mut(&id).unwrap().secondary_inventories.push(inv);
            }
        }
        let wo = &self.wos[&id];
        o.position = wo.position;
        o.rotation = wo.rotation;
        o.wo = Some(id);
        let active = wo.planet_hash() == self.planet_hash();
        for class in ["GroupNetworkContainer", "ConstructibleProxy"] {
            o.init(class, "_groupHash", enc::int(hash));
            o.init(class, "_active", enc::bool(active));
        }
        // ColorProxy.OnNetworkSpawn: no colour stays (0,0,0,0) until WorldObjectColor sets one.
        let colored = wo.color[0] > 0.0 || wo.color[1] > 0.0 || wo.color[2] > 0.0;
        o.init("ColorProxy", "_color", enc::color(if colored { [wo.color[0], wo.color[1], wo.color[2], 1.0] } else { [0.0; 4] }));
        o.init("SettingProxy", "_setting", enc::int(wo.setting));
        o.init("TextProxy", "_text", enc::fixed_string(&wo.text, 128));
        let linked: Vec<Vec<u8>> = wo.linked_groups.iter().flatten().map(|g| enc::int(self.data.groups[*g].hash)).collect();
        o.init_list("LinkedGroupsProxy", "_linkedGroupsHash", linked);
        o.init("LinkedPlanetProxy", "_linkedPlanet", enc::int(wo.planet_linked));
        if let Some(panels) = wo.panels.clone() {
            let mut i = 0;
            for b in o.behaviours.iter_mut().filter(|b| b.class == "Panel") {
                if let (Some(p), Some(v)) = (panels.get(i), b.vars.iter_mut().find(|v| v.name == "_currentSubPanelType")) {
                    v.value = VarValue::Plain(enc::enumeration(*p));
                }
                i += 1;
            }
        }
        let keep_server_ownership = !own_prefab && crate::objects::container_keeps_server_ownership(self, group);
        if owner != 0 && !keep_server_ownership {
            o.owner = owner;
            o.flags |= DONT_DESTROY_WITH_OWNER;
        }
        let net_id = self.net.spawn(o);
        self.wos.get_mut(&id).unwrap().net = Some(net_id);
        if constructible && !from_db && energy != 0.0 {
            crate::energy::update_all_energy_requesters(self);
        }
        crate::hooks::on_instantiated(self, id);
        Some(net_id)
    }

    /// DestroyWorldObject (server side).
    pub fn destroy_world_object(&mut self, id: i32, destroy_go: bool) {
        if !self.wos.contains_key(&id) {
            return;
        }
        self.remove_from_inventory_when_deleted(id);
        if is_from_scene(id) {
            return;
        }
        self.change_state_index(id);
        let Some(wo) = self.wos.shift_remove(&id) else { return };
        self.constructed.shift_remove(&id);
        self.pickables.shift_remove(&id);
        let hash = self.data.groups[wo.group].hash;
        if !wo.dont_save {
            self.count_list(false, "_constructedWorldObjectsGroups", hash);
            self.count_list(false, "_allWorldObjectsGroups", hash);
        }
        crate::hooks::on_destroyed(self, &wo);
        if self.data.groups[wo.group].constructible && self.data.groups[wo.group].unit("Energy") != 0.0 {
            crate::energy::update_all_energy_requesters(self);
        }
        if destroy_go {
            if let Some(net) = wo.net {
                self.net.despawn(net);
            }
        }
        if wo.linked_inventory > 0 {
            self.destroy_inventory(wo.linked_inventory, destroy_go);
        }
        for inv in &wo.secondary_inventories {
            self.destroy_inventory(*inv, destroy_go);
        }
    }

    pub fn destroy_world_objects(&mut self, ids: &[i32], destroy_go: bool) {
        for id in ids {
            self.destroy_world_object(*id, destroy_go);
        }
    }

    fn destroy_go_rpc(&mut self, to: Target, id: i32, disolve: f32) {
        let Some(group) = self.wos.get(&id).map(|w| self.data.groups[w.group].hash) else { return };
        self.net.rpc_scene(to, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc", &[json!(id), json!(group), json!(disolve)]);
    }

    /// DestroyWorldObjectGOOnAllClients: removes what stands for the object in everyone's scene.
    pub fn destroy_go_on_all_clients(&mut self, id: i32, disolve: f32) {
        let Some(wo) = self.wos.get(&id) else { return };
        match wo.net {
            Some(net) => self.net.despawn(net),
            None => self.destroy_go_rpc(Target::All, id, disolve),
        }
        if let Some(wo) = self.wos.get_mut(&id) {
            wo.net = None;
        }
    }

    /// DropOnFloorImplentation.
    #[allow(clippy::too_many_arguments)]
    pub fn drop_on_floor(&mut self, id: i32, position: [f32; 3], rotation: [f32; 4], drop_size: f32, sound: bool, nearest: bool, setting: i32, despawn: f32, disolve: bool, sender: u64) {
        let planet = self.planet_hash();
        let Some(wo) = self.wos.get_mut(&id) else { return };
        wo.position = position;
        wo.rotation = rotation;
        wo.planet = planet;
        if setting != 0 {
            wo.setting = setting;
        }
        self.update_save_state(id);
        crate::instances::check_dropped_object(self, id);
        // A host's own drops (sender 0) are simulated by the host's client; here the nearest
        // client stands in for it.
        let owner = if nearest || sender == 0 { self.nearest_client(position) } else { sender };
        let Some(net) = self.instantiate(id, false, owner) else { return };
        if rotation == [0.0, 0.0, 0.0, 1.0] {
            let turn = self.rng.range(90, 180) as f32;
            let r = crate::geometry::rotate_y(rotation, turn);
            self.net.move_object(net, position, Some(r));
        }
        self.net.rpc(Target::Only(owner), net, "GroupNetworkContainer.StartAndStopPhysicsAfterDelayClientRpc", &[json!(drop_size), json!(sound), json!(despawn > f32::EPSILON)]);
        if despawn > f32::EPSILON {
            crate::scene::schedule(self, despawn, crate::scene::Timer::Despawn(id));
        }
        if disolve {
            self.net.rpc(Target::All, net, "GroupNetworkBase.StartDisolveAnimationClientRpc", &[]);
        }
    }

    pub fn create_and_drop_on_floor(&mut self, group: usize, position: [f32; 3], drop_size: f32, save: bool, nearest: bool, despawn: f32, sender: u64) {
        let id = self.create_world_object(group, 0, save);
        self.drop_on_floor(id, position, [0.0, 0.0, 0.0, 1.0], drop_size, false, nearest, 0, despawn, false, sender);
    }

    /// RetrieveResourcesFromDeconstructionServerRpc.
    pub fn retrieve_resources_from_deconstruction(&mut self, wo: i32, group_hash: i32, player_inventory: i32, drop: [f32; 3], client: u64) {
        if self.save.settings["freeCraft"].as_bool() == Some(true) {
            return;
        }
        let Some(group) = self.data.group_by_hash(group_hash) else { return };
        let mut items: Vec<i32> = Vec::new();
        for ingredient in self.data.groups[group].recipe.clone() {
            items.push(self.create_world_object(ingredient, 0, true));
        }
        if let Some(panels) = self.wos.get(&wo).and_then(|w| w.panels.clone()) {
            for p in panels {
                if let Some(panel_group) = crate::panels::panel_group(self, p) {
                    for ingredient in self.data.groups[panel_group].recipe.clone() {
                        items.push(self.create_world_object(ingredient, 0, true));
                    }
                }
            }
        }
        if self.data.groups[group].bool("lootRecipeOnDeconstruct") && !crate::unlocks::is_globally_unlocked(self, group) {
            if let Some(blueprint) = crate::unlocks::blueprint_group(self) {
                let b = self.create_world_object(blueprint, 0, true);
                self.wos.get_mut(&b).unwrap().linked_groups = Some(vec![group]);
                items.push(b);
            }
        }
        let (mut dropped, mut stored) = (Vec::new(), Vec::new());
        for item in items {
            let hash = self.data.groups[self.wos[&item].group].hash;
            let full = self.inventories.get(&player_inventory).is_none_or(|i| i.is_full());
            if player_inventory == -1 || full {
                self.drop_on_floor(item, [drop[0], drop[1] + 1.0, drop[2]], [0.0, 0.0, 0.0, 1.0], 0.0, true, true, 0, 0.0, false, client);
                dropped.push(hash);
            } else {
                // AddItemToInventory from the server: the change goes to every watcher.
                self.add_existing_item(item, player_inventory, true, 0);
                stored.push(hash);
            }
        }
        if !dropped.is_empty() || !stored.is_empty() {
            self.net.rpc_scene(Target::Only(client), "WorldObjectsHandler.DisplayDeconstructResultClientRpc", &[json!(dropped), json!(stored)]);
        }
    }

    /// The WorldObjectsHandler RPCs; false when `name` is not one of them.
    pub fn world_objects_rpc(&mut self, client: u64, name: &str, a: &Args) -> bool {
        let me = Target::Only(client);
        match name {
            "WorldObjectsHandler.HasWorldObjectAtPositionServerRpc" => {
                let p = a.v3(0);
                let found = self.wos.values().any(|w| dist2(w.position, p) < f32::EPSILON);
                self.net.rpc_scene(me, "WorldObjectsHandler.HasWorldObjectAtPositionClientRpc", &[json!(found)]);
            }
            "WorldObjectsHandler.GetGameObjectFromWorldObjectServerRpc" => {
                let net = self.wos.get(&a.i(0)).and_then(|w| w.net).unwrap_or(0);
                self.net.rpc_scene(me, "WorldObjectsHandler.GetGameObjectFromWorldObjectClientRpc", &[json!(net)]);
            }
            "WorldObjectsHandler.AddOneToTotalCraftServerRpc" => {
                for c in self.clients.keys().copied().collect::<Vec<_>>() {
                    if let Some(t) = self.total_crafted.get_mut(&c) {
                        *t += 1;
                    }
                }
                // The save's stats are the host's _totalCraft, which AddOneToTotalCraftClientRpc
                // counts up for every craft; this server stands in for a host always present.
                self.crafted_total += 1;
                self.net.rpc_scene(Target::All, "WorldObjectsHandler.AddOneToTotalCraftClientRpc", &[]);
            }
            "WorldObjectsHandler.DestroyWorldObjectServerRpc" => self.destroy_world_object(a.i(0), a.b(1)),
            "WorldObjectsHandler.DestroyWorldObjectFromSceneServerRpc" => {
                let (id, hash, disolve) = (a.i(0), a.i(1), a.f(2));
                let mut created = false;
                if !self.wos.contains_key(&id) {
                    if let Some(g) = self.data.group_by_hash(hash) {
                        self.create_world_object(g, id, true);
                        created = true;
                    }
                }
                let mut success = false;
                if self.wos.get(&id).is_some_and(|w| created || w.is_placed()) {
                    let w = self.wos.get_mut(&id).unwrap();
                    w.dont_save = false;
                    w.reset_position();
                    self.destroy_go_on_all_clients(id, disolve);
                    success = true;
                }
                self.net.rpc_scene(me, "WorldObjectsHandler.DestroyWorldObjectFromSceneResultClientRpc", &[json!(success)]);
            }
            "WorldObjectsHandler.DisableWorldObjectFromSceneServerRpc" => {
                let (id, hash, update_lists, generate_at) = (a.i(0), a.i(1), a.b(2), a.v3(3));
                let mut created = false;
                if !self.wos.contains_key(&id) {
                    if let Some(g) = self.data.group_by_hash(hash) {
                        self.create_world_object(g, id, true);
                        created = true;
                    }
                }
                if self.wos.get(&id).is_some_and(|w| created || w.is_placed()) {
                    let w = self.wos.get_mut(&id).unwrap();
                    w.dont_save = false;
                    w.reset_position();
                    let group = w.group;
                    if update_lists {
                        self.update_save_state(id);
                    }
                    if generate_at.iter().map(|v| v * v).sum::<f32>().sqrt() > f32::EPSILON {
                        self.create_and_drop_on_floor(group, generate_at, 0.0, true, true, 0.0, client);
                    }
                    crate::scene::on_world_object_from_scene_destroyed(self, id);
                }
            }
            "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc" => {
                let id = a.i(0);
                if self.wos.get(&id).is_some_and(|w| !w.dont_save) {
                    self.destroy_go_rpc(me, id, 0.0);
                }
            }
            "WorldObjectsHandler.RetrieveResourcesFromDeconstructionServerRpc" => {
                self.retrieve_resources_from_deconstruction(a.i(0), a.i(1), a.i(2), a.v3(3), a.u(4));
            }
            "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc" => {
                let (hash, position, rotation, disolve, check, save, icon) = (a.i(0), a.v3(1), a.q(2), a.b(3), a.b(4), a.b(5), a.b(6));
                if check && self.wos.values().any(|w| dist2(w.position, position) < f32::EPSILON) {
                    self.net.rpc_scene(me, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc", &[json!(0)]);
                    return true;
                }
                let Some(group) = self.data.group_by_hash(hash) else {
                    self.net.rpc_scene(me, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc", &[json!(0)]);
                    return true;
                };
                let id = self.new_world_object_id();
                self.create_world_object(group, id, save);
                let planet = self.planet_hash();
                let w = self.wos.get_mut(&id).unwrap();
                w.position = position;
                w.rotation = rotation;
                w.planet = planet;
                let owner = if self.data.groups[group].usable_type() == "Buildable" { self.nearest_client(position) } else { 0 };
                let net = self.instantiate(id, false, owner).unwrap_or(0);
                let has_base = net != 0 && (self.net.behaviour_index(net, "ConstructibleProxy").is_some() || self.net.behaviour_index(net, "GroupNetworkContainer").is_some());
                if has_base {
                    if disolve {
                        self.net.rpc(Target::All, net, "GroupNetworkBase.StartDisolveAnimationClientRpc", &[]);
                    }
                    if icon {
                        self.constructed.shift_remove(&id);
                        self.net.rpc(Target::All, net, "GroupNetworkBase.AddDeconstructIconClientRpc", &[]);
                    }
                }
                self.net.rpc_scene(me, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc", &[json!(net)]);
            }
            "WorldObjectsHandler.CreateAndDropOnFloorServerRpc" => {
                if let Some(group) = self.data.group_by_hash(a.i(0)) {
                    self.create_and_drop_on_floor(group, a.v3(1), a.f(2), a.b(3), a.b(4), a.f(5), client);
                }
            }
            "WorldObjectsHandler.DropOnFloorServerRpc" => {
                self.drop_on_floor(a.i(0), a.v3(1), [0.0, 0.0, 0.0, 1.0], a.f(2), a.b(3), a.b(4), a.i(5), a.f(6), false, client);
            }
            "WorldObjectsHandler.DropOnFloorWithRotationServerRpc" => {
                self.drop_on_floor(a.i(0), a.v3(1), a.q(2), a.f(3), a.b(4), a.b(5), a.i(6), a.f(7), true, client);
            }
            "WorldObjectsHandler.GetEnergyGroupDataServerRpc" => {
                let data = crate::energy::energy_group_data(self);
                self.net.rpc_scene(me, "WorldObjectsHandler.GetEnergyGroupDataClientRpc", &[json!(data)]);
            }
            "WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc" => {
                let (chance, hash, id, inventory, position, rotation) = (a.f(0), a.i(1), a.i(2), a.i(3), a.v3(4), a.q(5));
                if let Some(w) = self.wos.get(&id).filter(|w| !w.dont_save) {
                    if !w.is_placed() || dist2(w.position, position).sqrt() > 2.0 {
                        self.destroy_go_rpc(me, id, 0.0);
                    }
                    return true;
                }
                if !self.wos.contains_key(&id) {
                    let Some(g) = self.data.group_by_hash(hash) else { return true };
                    self.create_world_object(g, id, true);
                }
                self.wos.get_mut(&id).unwrap().dont_save = false;
                if chance >= self.rng.rangef(1.0, 100.0) {
                    let planet = self.planet_hash();
                    let w = self.wos.get_mut(&id).unwrap();
                    w.position = position;
                    w.rotation = rotation;
                    w.planet = planet;
                    if inventory >= 0 {
                        w.linked_inventory = inventory;
                    }
                } else {
                    self.destroy_go_rpc(me, id, 0.0);
                }
            }
            "WorldObjectsHandler.GetGeneticsDetailsServerRpc" => {
                let ids = a.ints(0).unwrap_or_default();
                let (mut types, mut values) = (Vec::new(), Vec::new());
                let mut ok = true;
                for id in &ids {
                    match self.wos.get(id) {
                        Some(w) => {
                            types.push(w.trait_type);
                            values.push(crate::genetics::trait_value(self, w));
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    types.resize(ids.len(), 0);
                    values.resize(ids.len(), 0);
                }
                self.net.rpc_scene(me, "WorldObjectsHandler.GetGeneticsDetailsClientRpc", &[json!(ok), json!(types), json!(values)]);
            }
            "WorldObjectsHandler.SetSceneWorldObjectGrowthServerRpc" => {
                let (id, hash, count) = (a.i(0), a.i(1), a.0.get(2).cloned().unwrap_or(json!([0, 0])));
                let count = [count[0].as_i64().unwrap_or(0) as i32, count[1].as_i64().unwrap_or(0) as i32];
                self.set_scene_growth(id, hash, count);
                self.net.rpc_scene(Target::All, "WorldObjectsHandler.SetSceneWorldObjectGrowthClientRpc", &[json!(id), json!(hash), json!(count)]);
            }
            "WorldObjectsHandler.RefreshSceneWorldObjectGrowthServerRpc" => {
                let id = a.i(0);
                if let Some(w) = self.wos.get(&id) {
                    let (hash, count) = (self.data.groups[w.group].hash, w.count);
                    self.net.rpc_scene(me, "WorldObjectsHandler.SetSceneWorldObjectGrowthClientRpc", &[json!(id), json!(hash), json!(count)]);
                }
            }
            _ => return false,
        }
        true
    }

    fn set_scene_growth(&mut self, id: i32, hash: i32, count: [i32; 2]) {
        if !self.wos.contains_key(&id) {
            let Some(g) = self.data.group_by_hash(hash) else { return };
            self.create_world_object(g, id, true);
        }
        let w = self.wos.get_mut(&id).unwrap();
        w.count = count;
        w.growth = if count[1] != 0 { count[0] as f32 / count[1] as f32 * 100.0 } else { 0.0 };
    }
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// GroupNetworkContainer.CreateChild keeps server ownership when the item's prefab has its own
/// physics body (a Rigidbody): the server, not the dropping client, then simulates it.
pub fn container_keeps_server_ownership(game: &Game, group: usize) -> bool {
    game.data.group_prefabs[&game.data.groups[group].id]["rigidbody"].as_bool().unwrap_or(false)
}
