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
        let Some(VarValue::List(items)) = self.net.get(object, HANDLER, var) else { return };
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

    /// The server part of CreateAndInstantiateWorldObjectServerRpc: a new world object of the
    /// group at a place, spawned; returns its id and network object.
    pub fn create_and_instantiate(&mut self, group: usize, position: [f32; 3], rotation: [f32; 4], disolve: bool, save: bool, icon: bool) -> (i32, u64) {
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
                self.constructed.remove(&id);
                self.net.rpc(Target::All, net, "GroupNetworkBase.AddDeconstructIconClientRpc", &[]);
            }
        }
        (id, net)
    }

    /// InstantiateWorldObject: the world object's network object, with its variables from
    /// the world object, its inventories, and its spawn for everyone.
    pub fn instantiate(&mut self, id: i32, from_db: bool, owner: u64) -> Option<u64> {
        // Already in the world: a second network object would duplicate it.
        if let Some(net) = self.wos.get(&id)?.net.filter(|n| self.net.objects.contains_key(n)) {
            return Some(net);
        }
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
        let Some(def) = self.data.layouts.get(&key) else {
            eprintln!("no network layout for {key}");
            return None;
        };
        let mut o = NetObj::from_layout(&def.layout(self.tick_number()));
        let no_group_hash = !def.group_hash_on_spawn;
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
        // Object.Instantiate makes a zero quaternion the identity.
        o.rotation = if wo.rotation == [0.0; 4] { [0.0, 0.0, 0.0, 1.0] } else { wo.rotation };
        o.wo = Some(id);
        let active = wo.planet_hash() == self.planet_hash();
        // InstantiateWorldObject and GroupNetworkBase.OnNetworkSpawn: an own prefab whose root
        // has no GroupNetworkBase, or no children, keeps group hash 0 (the escape pod).
        let keeps_zero = own_prefab && no_group_hash;
        for class in ["GroupNetworkContainer", "ConstructibleProxy"] {
            if !keeps_zero {
                o.init(class, "_groupHash", enc::int(hash));
            }
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
            // SetPanelsForNewlyInstantiatedWorldObject: Panel.ChangePanel per saved type, which
            // counts each one that changes the panel in _constructedPanelTypes (loads included).
            let mut changed = Vec::new();
            let mut i = 0;
            for b in o.behaviours.iter_mut().filter(|b| b.class == "Panel") {
                if let (Some(p), Some(v)) = (panels.get(i), b.vars.iter_mut().find(|v| v.name == "_currentSubPanelType")) {
                    let new = enc::enumeration(*p);
                    if !matches!(&v.value, VarValue::Plain(b) if *b == new) {
                        changed.push(*p);
                    }
                    v.value = VarValue::Plain(new);
                }
                i += 1;
            }
            for p in changed {
                self.panel_constructed(p);
            }
        }
        if constructible && !from_db {
            crate::panels::change_contiguous_panels(self, &mut o);
        }
        let keep_server_ownership = !own_prefab && crate::objects::container_keeps_server_ownership(self, group);
        // SpawnWithOwnership, then DontDestroyWithOwner: the spawn message goes out without the flag,
        // a client joining later gets it. A vehicle's GroupNetworkContainer.OnNetworkSpawn sets it
        // inside the spawn, so its message has it.
        let mut set_after_spawn = false;
        if owner != 0 && !keep_server_ownership {
            o.owner = owner;
            if self.data.prefab_components(&self.data.groups[group].id, "GroupNetworkContainer").first().is_some_and(|c| c["_alwaysKeepRigidBodyAndNetworkTransformActive"] == true) {
                o.flags |= DONT_DESTROY_WITH_OWNER;
            } else {
                set_after_spawn = true;
            }
        }
        let net_id = self.net.spawn(o);
        if set_after_spawn {
            self.net.objects.get_mut(&net_id).unwrap().flags |= DONT_DESTROY_WITH_OWNER;
        }
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
        let Some(wo) = self.wos.remove(&id) else { return };
        self.constructed.remove(&id);
        self.pickables.remove(&id);
        let hash = self.data.groups[wo.group].hash;
        if !wo.dont_save {
            self.count_list(false, "_constructedWorldObjectsGroups", hash);
            self.count_list(false, "_allWorldObjectsGroups", hash);
        }
        crate::hooks::on_destroyed(self, &wo);
        if self.data.groups[wo.group].constructible && self.data.groups[wo.group].unit("Energy") != 0.0 {
            crate::energy::update_all_energy_requesters(self);
        }
        // The game destroys an object's GameObject before its world object (Explode alone keeps it,
        // see meteo::explode go_off); a client that asks for the world object only, or destroys one
        // it dropped without taking it out of its inventory, would leave a network object that
        // stands for nothing, in every client's scene and in every join. Except a plant of a
        // grower (a Farm's, a VegetableGrower's): its game object is the machine's, and the game
        // leaves it when the items of the machine's inventory are destroyed (the parity sweep:
        // RemoveAndDestroyAllItemsFromInventory at a Farm2's deconstruction sends no destroy). It
        // going would also read as the plant being harvested (ActionGrabable.OnDestroy).
        if let Some(net) = wo.net.filter(|_| destroy_go || !self.ext.machines.grabbable(id)) {
            self.net.despawn(net);
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
        // The components on the GameObject go with it (Drone.OnDestroy).
        if wo.net.is_some() {
            crate::logistics::on_go_destroyed(self, id);
        }
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
        self.drop_on_floor_told(id, position, rotation, drop_size, sound, nearest, setting, despawn, disolve, sender, true);
    }

    /// `tell_server_kept`: whether an object with a physics body of its own (a drone, a rover)
    /// is announced with StartAndStopPhysicsAfterDelay: DropOnFloor and what the server creates
    /// itself (a recycler's output) do, a client's CreateAndDropOnFloor does not.
    #[allow(clippy::too_many_arguments)]
    fn drop_on_floor_told(&mut self, id: i32, position: [f32; 3], rotation: [f32; 4], drop_size: f32, sound: bool, nearest: bool, setting: i32, despawn: f32, disolve: bool, sender: u64, tell_server_kept: bool) {
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
            // WorldObjectAssociated.AutoSavePositionContinuously: a second later the object's
            // transform is the world object's, until the owner moves it.
            if let Some(w) = self.wos.get_mut(&id) {
                w.rotation = r;
            }
        }
        let group = self.wos.get(&id).map_or(0, |w| w.group);
        if tell_server_kept || !container_keeps_server_ownership(self, group) {
            self.net.rpc(Target::Only(owner), net, "GroupNetworkContainer.StartAndStopPhysicsAfterDelayClientRpc", &[json!(drop_size), json!(sound), json!(despawn > f32::EPSILON)]);
        }
        if despawn > f32::EPSILON {
            crate::scene::schedule(self, despawn, crate::scene::Timer::Despawn(id, net));
        }
        if disolve {
            self.net.rpc(Target::All, net, "GroupNetworkBase.StartDisolveAnimationClientRpc", &[]);
        }
    }

    pub fn create_and_drop_on_floor(&mut self, group: usize, position: [f32; 3], drop_size: f32, save: bool, nearest: bool, despawn: f32, sender: u64) -> i32 {
        let id = self.create_world_object(group, 0, save);
        self.drop_on_floor_told(id, position, [0.0, 0.0, 0.0, 1.0], drop_size, false, nearest, 0, despawn, false, sender, true);
        id
    }

    /// Rules: a building past its limit (limits.buildings) comes down at once, its materials
    /// back to the builder as a deconstruction gives them, with a word in the chat. (Letting it
    /// be built first returns the materials whatever the client took for it.)
    fn over_building_limit(&mut self, client: u64, id: i32, group: usize) {
        let Some(&max) = self.rules.limits.buildings.get(&self.data.groups[group].id) else { return };
        // The buildings that stand: the game's count of them (group_count) stays one too high for good
        // for each one made without being saved, and so would block the limit for everyone.
        let standing = self.wos.values().filter(|w| w.group == group && w.is_placed() && !is_from_scene(w.id)).count();
        if !self.wos.contains_key(&id) || standing <= max as usize {
            return;
        }
        if let Some(inventory) = self.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.inventory) {
            let position = self.wos[&id].position;
            self.retrieve_resources_from_deconstruction(id, self.data.groups[group].hash, inventory, position, client);
        }
        self.destroy_go_on_all_clients(id, 0.0);
        self.destroy_world_object(id, false);
        let message = format!("This world allows {max} of {}: it was taken down and its materials returned", self.data.groups[group].id);
        self.net.rpc_scene(Target::Only(client), "ChatHandler.ReceiveChatMessageClientRpc", &[json!(0), json!(message)]);
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
            "WorldObjectsHandler.DestroyWorldObjectServerRpc" => {
                if !self.others_item(client, a.i(0)) {
                    self.destroy_world_object(a.i(0), a.b(1));
                }
            }
            "WorldObjectsHandler.DestroyWorldObjectFromSceneServerRpc" => {
                let (id, hash, disolve) = (a.i(0), a.i(1), a.f(2));
                let mut created = false;
                // Scene world objects have positive ids; 0 would make a fresh one.
                if id > 0 && !self.wos.contains_key(&id) {
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
                // Scene world objects have positive ids; 0 would make a fresh one.
                if id > 0 && !self.wos.contains_key(&id) {
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
                if !self.others_inventory(client, a.i(2)) {
                    self.retrieve_resources_from_deconstruction(a.i(0), a.i(1), a.i(2), a.v3(3), a.u(4));
                }
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
                let (id, net) = self.create_and_instantiate(group, position, rotation, disolve, save, icon);
                self.net.rpc_scene(me, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc", &[json!(net)]);
                self.over_building_limit(client, id, group);
            }
            "WorldObjectsHandler.CreateAndDropOnFloorServerRpc" => {
                if let Some(group) = self.data.group_by_hash(a.i(0)) {
                    let despawn = a.f(5);
                    // A client creating a rover or a drone on the floor is told nothing of its physics.
                    let id = self.create_world_object(group, 0, a.b(3));
                    self.drop_on_floor_told(id, a.v3(1), [0.0, 0.0, 0.0, 1.0], a.f(2), false, a.b(4), 0, despawn, false, client, false);
                    // Rules: what the game drops with a despawn time is a meteor's material.
                    if let Some(seconds) = self.rules.meteo.material_despawn_seconds.filter(|_| despawn > f32::EPSILON) {
                        crate::scene::set_despawn(self, id, seconds);
                    }
                }
            }
            "WorldObjectsHandler.DropOnFloorServerRpc" | "WorldObjectsHandler.DropOnFloorWithRotationServerRpc" if self.others_item(client, a.i(0)) => {}
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
                if id <= 0 {
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
                    // Never a player's: destroying the object would take the inventory along.
                    if inventory >= 0 && self.inventory_player(inventory).is_none() {
                        self.wos.get_mut(&id).unwrap().linked_inventory = inventory;
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
        if id <= 0 {
            return;
        }
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

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use crate::net::Target;
    use crate::testkit::*;

    const POD: i32 = 208217122;
    /// A scene object's id (below the player-made ids) that the save does not hold.
    const SCENE: i32 = 101_234_567;

    /// DropOnFloorImplentation turns an unrotated drop by 90 to 179 degrees about Y, and the
    /// object's autosave keeps that turn in the world object (and so in the save).
    #[test]
    fn an_unrotated_drop_is_saved_turned_about_y() {
        let Some((mut g, _)) = world() else { return };
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        let r = g.wos[&iron].rotation;
        assert_eq!(r, g.net.objects[&net].rotation);
        let degrees = 2.0 * r[1].atan2(r[3]).to_degrees();
        assert!((r[0], r[2]) == (0.0, 0.0) && (90.0..180.0).contains(&degrees), "{r:?}");
        let kept = [0.0, 0.70710677, 0.0, 0.70710677];
        let other = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(other, [561.0, 3.0, 497.0], kept, 0.0, false, false, 0, 0.0, false, 1);
        assert_eq!(g.wos[&other].rotation, kept, "a rotation the caller gave stays");
    }

    /// A drone and a rover have a physics body of their own: dropping one, or the server making
    /// one, starts the physics on the dropper's client; a client creating one on the floor is
    /// told nothing (the real host). An iron bar is announced every way.
    #[test]
    fn a_client_creating_a_drone_or_rover_on_the_floor_is_not_told_to_start_its_physics() {
        let Some((mut g, _)) = world() else { return };
        const RPC: &str = "GroupNetworkContainer.StartAndStopPhysicsAfterDelayClientRpc";
        for (name, bodied) in [("Iron", false), ("Drone1", true), ("VehicleTruck", true)] {
            let group = g.data.group(name).unwrap();
            g.net.out.clear();
            let hash = g.data.groups[group].hash;
            send_scene(&mut g, 1, "WorldObjectsHandler.CreateAndDropOnFloorServerRpc", &[json!(hash), json!([561.0, 3.0, 497.0]), json!(0.0), json!(true), json!(false), json!(0.0)]);
            assert_eq!(sent(&g, RPC).is_empty(), bodied, "{name} created by a client");
            g.net.out.clear();
            g.create_and_drop_on_floor(group, [561.0, 3.0, 497.0], 0.0, true, false, 0.0, 0);
            assert!(!sent(&g, RPC).is_empty(), "{name} created by the server");
            let item = g.create_world_object(group, 0, true);
            g.net.out.clear();
            g.drop_on_floor(item, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, true, false, 0, 0.0, false, 1);
            assert!(!sent(&g, RPC).is_empty(), "{name} dropped");
        }
    }

    /// The flags of the object a spawn message announced (the last one queued).
    fn spawned_flags(g: &crate::game::Game) -> u16 {
        let body = &g.net.out.iter().rev().find(|m| m.1 == crate::protocol::CREATE_OBJECT).unwrap().2;
        let mut r = crate::wire::Reader::new(body);
        r.u8().unwrap();
        crate::protocol::read_scene_object(&mut r).unwrap().flags
    }

    /// A destroyed world object leaves no network object behind, whatever the destroy flag says:
    /// an item dropped and destroyed with RemoveItemFromInventory (it was never taken out), or
    /// destroyed with DestroyWorldObject(false).
    #[test]
    fn a_destroyed_world_object_takes_its_network_object_along() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let before = g.net.objects.len();
        let item = give(&mut g, "Iron", bag);
        send_scene(&mut g, 1, "WorldObjectsHandler.DropOnFloorServerRpc", &[json!(item), json!([561.0, 3.0, 497.0]), json!(0.0), json!(false), json!(false), json!(0), json!(0.0)]);
        assert_eq!(g.net.objects.len(), before + 1, "dropped: it has a network object");
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(item), json!(bag), json!(true)]);
        assert!(!g.wos.contains_key(&item));
        assert_eq!(g.net.objects.len(), before, "no network object of a gone item");
        let other = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(other, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        assert_eq!(g.net.objects.len(), before + 1);
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectServerRpc", &[json!(other), json!(false)]);
        assert_eq!(g.net.objects.len(), before);
    }

    /// SpawnWithOwnership sends the spawn without DontDestroyWithOwner, which the game sets right
    /// after (a client joining later gets it); a vehicle's OnNetworkSpawn sets it inside the spawn.
    #[test]
    fn dont_destroy_with_owner_follows_the_spawn_message_for_items_but_not_vehicles() {
        let Some((mut g, _)) = world() else { return };
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        assert_eq!(g.net.objects[&net].owner, 1);
        assert_eq!(spawned_flags(&g) & super::DONT_DESTROY_WITH_OWNER, 0, "the message");
        assert_ne!(g.net.objects[&net].flags & super::DONT_DESTROY_WITH_OWNER, 0, "the object, for a late joiner");
        let (_, truck) = g.create_and_instantiate(g.data.group("VehicleTruck").unwrap(), [570.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], false, true, false);
        assert_eq!(g.net.objects[&truck].owner, 1);
        assert_ne!(spawned_flags(&g) & super::DONT_DESTROY_WITH_OWNER, 0);
    }

    /// The escape pod's prefab gives its group to no GroupNetworkBase on its root: the real
    /// host spawns it with group hash 0, other buildings with theirs.
    #[test]
    fn prefabs_without_a_root_group_base_keep_group_hash_zero() {
        let Some((mut g, _)) = world() else { return };
        std::sync::Arc::make_mut(&mut g.data.layouts).get_mut("EscapePod").unwrap().group_hash_on_spawn = false;
        let (_, pod) = place(&mut g, "EscapePod", [570.0, 3.0, 500.0]);
        let (_, chest) = place(&mut g, "Container1", [575.0, 3.0, 500.0]);
        let hash = |g: &crate::game::Game, net| var(g, net, "ConstructibleProxy", "_groupHash");
        assert_eq!(hash(&g, pod), crate::net::enc::int(0));
        assert_eq!(hash(&g, chest), crate::net::enc::int(g.data.groups[g.data.group("Container1").unwrap()].hash));
    }

    #[test]
    fn world_object_queries_answer_the_asking_client() {
        let Some((mut g, _)) = world() else { return };
        join(&mut g, 2, "Bob");
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.HasWorldObjectAtPositionServerRpc", &[json!([558.2, 2.47, 494.1])]);
        send_scene(&mut g, 1, "WorldObjectsHandler.HasWorldObjectAtPositionServerRpc", &[json!([1.0, 2.0, 3.0])]);
        let answers: Vec<(Target, Value)> = sent(&g, "WorldObjectsHandler.HasWorldObjectAtPositionClientRpc").into_iter().map(|s| (s.to, s.args[0].clone())).collect();
        assert_eq!(answers, vec![(Target::Only(1), json!(true)), (Target::Only(1), json!(false))]);

        let pod = g.wos[&POD].net.unwrap();
        send_scene(&mut g, 2, "WorldObjectsHandler.GetGameObjectFromWorldObjectServerRpc", &[json!(POD)]);
        send_scene(&mut g, 2, "WorldObjectsHandler.GetGameObjectFromWorldObjectServerRpc", &[json!(123)]);
        let answers: Vec<(Target, Value)> = sent(&g, "WorldObjectsHandler.GetGameObjectFromWorldObjectClientRpc").into_iter().map(|s| (s.to, s.args[0].clone())).collect();
        assert_eq!(answers, vec![(Target::Only(2), json!(pod)), (Target::Only(2), json!(0))], "no object: network id 0");
    }

    #[test]
    fn energy_group_data_sums_each_groups_generation_on_this_planet() {
        let Some((mut g, _)) = world() else { return };
        place(&mut g, "EnergyGenerator1", [570.0, 3.0, 500.0]);
        place(&mut g, "EnergyGenerator1", [575.0, 3.0, 500.0]);
        place(&mut g, "CraftStation1", [580.0, 3.0, 500.0]);
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.GetEnergyGroupDataServerRpc", &[]);
        let rows = one(&g, "WorldObjectsHandler.GetEnergyGroupDataClientRpc")[0].as_array().unwrap().clone();
        let unit = |group: &str| g.data.groups[g.data.group(group).unwrap()].unit("Energy");
        let row = |group: &str| rows.iter().find(|r| r[0] == hash(&g, group)).cloned();
        assert_eq!(row("EnergyGenerator1"), Some(json!([hash(&g, "EnergyGenerator1"), 2.0 * unit("EnergyGenerator1"), 2])));
        assert_eq!(row("CraftStation1"), Some(json!([hash(&g, "CraftStation1"), unit("CraftStation1"), 1])), "consumers count negative");
        assert!(row("EscapePod").is_none(), "no energy, no row");
    }

    #[test]
    fn genetics_details_list_each_traits_type_and_value_or_nothing() {
        let Some((mut g, _)) = world() else { return };
        let group = g.data.group(crate::genetics::TRAIT_GROUP).unwrap();
        let (a, b) = (g.create_world_object(group, 0, true), g.create_world_object(group, 0, true));
        g.wos.get_mut(&a).unwrap().trait_type = 7;
        g.wos.get_mut(&a).unwrap().trait_value = 3;
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.GetGeneticsDetailsServerRpc", &[json!([a, b])]);
        let (tb, vb) = (g.wos[&b].trait_type, crate::genetics::trait_value(&g, &g.wos[&b]));
        assert_eq!(one(&g, "WorldObjectsHandler.GetGeneticsDetailsClientRpc"), vec![json!(true), json!([7, tb]), json!([3, vb])]);
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.GetGeneticsDetailsServerRpc", &[json!([a, 42])]);
        assert_eq!(one(&g, "WorldObjectsHandler.GetGeneticsDetailsClientRpc"), vec![json!(false), json!([7, 0]), json!([3, 0])], "the game stops at the unknown one");
    }

    /// Building (CreateAndInstantiate), dropping (DropOnFloor, with and without a rotation) and
    /// destroying, each as the save and the clients see it.
    #[test]
    fn objects_are_built_dropped_and_destroyed_as_in_the_game() {
        let Some((mut g, _)) = world() else { return };
        let (inventory, _) = backpack(&g, 1);
        let generator = hash(&g, "EnergyGenerator1");
        let built_before = g.group_count(true, g.data.group("EnergyGenerator1").unwrap());
        // On the pod's spot with the check on, and of a group that does not exist: nothing.
        for (hash, position) in [(generator, [558.2, 2.47, 494.1]), (12345, [600.0, 3.0, 500.0])] {
            send_scene(&mut g, 1, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(hash), json!(position), json!([0, 0, 0, 1]), json!(false), json!(true), json!(true), json!(false)]);
            assert_eq!(one(&g, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc"), vec![json!(0)]);
            g.net.out.clear();
        }
        send_scene(&mut g, 1, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(generator), json!([600.0, 3.0, 500.0]), json!([0, 0, 0, 1]), json!(true), json!(true), json!(true), json!(false)]);
        let net = one(&g, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc")[0].as_u64().unwrap();
        let built = g.net.objects[&net].wo.unwrap();
        assert_eq!(sent(&g, "GroupNetworkBase.StartDisolveAnimationClientRpc").len(), 1, "disolve");
        assert_eq!(g.group_count(true, g.wos[&built].group), built_before + 1);
        assert_eq!(saved_object(&mut g, built).unwrap()["pos"], "600,3,500");

        // An Iron from the backpack dropped by Ann: hers to simulate, saved where it fell.
        let iron = give(&mut g, "Iron", inventory);
        g.inventory_remove(inventory, iron);
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.DropOnFloorServerRpc", &[json!(iron), json!([561.0, 3.0, 497.0]), json!(0.5), json!(true), json!(false), json!(0), json!(0.0)]);
        let dropped = g.wos[&iron].net.unwrap();
        assert_eq!(g.net.objects[&dropped].owner, 1);
        assert_eq!(one(&g, "GroupNetworkContainer.StartAndStopPhysicsAfterDelayClientRpc"), vec![json!(0.5), json!(true), json!(false)]);
        assert_ne!(g.net.objects[&dropped].rotation, [0.0, 0.0, 0.0, 1.0], "an unrotated drop gets a random turn");
        assert_eq!(saved_object(&mut g, iron).unwrap()["pos"], "561,3,497");
        // With a rotation, the rotation is kept.
        let ice = give(&mut g, "ice", inventory);
        g.inventory_remove(inventory, ice);
        let turned = crate::geometry::from_euler_y(30.0);
        send_scene(&mut g, 1, "WorldObjectsHandler.DropOnFloorWithRotationServerRpc", &[json!(ice), json!([562.0, 3.0, 497.0]), json!(turned), json!(0.5), json!(false), json!(false), json!(0), json!(0.0)]);
        assert_eq!(g.wos[&ice].rotation, turned);

        // Destroyed: gone from the world, the clients' scenes, the counts and the save.
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectServerRpc", &[json!(built), json!(true)]);
        assert!(!g.wos.contains_key(&built) && !g.net.objects.contains_key(&net));
        assert_eq!(g.group_count(true, g.data.group("EnergyGenerator1").unwrap()), built_before);
        assert!(saved_object(&mut g, built).is_none());
        // An item in an inventory comes out of it.
        let carried = give(&mut g, "Iron", inventory);
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectServerRpc", &[json!(carried), json!(false)]);
        assert!(!g.inventories[&inventory].items.contains(&carried) && !g.wos.contains_key(&carried));
    }

    /// Scene objects (ids below the player-made ones) players took, disabled or regrew: the
    /// server keeps them as the game does, and a reloaded world tells joining clients again.
    #[test]
    fn scene_objects_players_change_stay_changed_after_a_reload() {
        let Some((mut g, _)) = world() else { return };
        let iron = hash(&g, "Iron");
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectFromSceneServerRpc", &[json!(SCENE), json!(iron), json!(1.5)]);
        assert_eq!(one(&g, "WorldObjectsHandler.DestroyWorldObjectFromSceneResultClientRpc"), vec![json!(true)]);
        assert_eq!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc")[0].args, vec![json!(SCENE), json!(iron), json!(1.5)], "everyone's copy goes");
        assert!(!g.wos[&SCENE].is_placed() && !g.wos[&SCENE].dont_save);
        // Again: it is no longer placed, so the second asker gets false.
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectFromSceneServerRpc", &[json!(SCENE), json!(iron), json!(0.0)]);
        assert_eq!(one(&g, "WorldObjectsHandler.DestroyWorldObjectFromSceneResultClientRpc"), vec![json!(false)]);

        // Disabled with an item to generate where it stood: the item drops there.
        let (disabled, magnesium) = (SCENE + 1, hash(&g, "Magnesium"));
        send_scene(&mut g, 1, "WorldObjectsHandler.DisableWorldObjectFromSceneServerRpc", &[json!(disabled), json!(magnesium), json!(true), json!([565.0, 3.0, 495.0])]);
        let drop = g.wos.values().find(|w| w.id != disabled && g.data.groups[w.group].hash == magnesium).expect("a Magnesium dropped");
        assert_eq!(drop.position, [565.0, 3.0, 495.0]);
        assert!(!g.wos[&disabled].is_placed() && !g.wos[&disabled].dont_save);

        // A grown scene plant: its count and growth are everyone's and the save's.
        let (grown, vegetable) = (SCENE + 2, hash(&g, "Vegetable0Seed"));
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.SetSceneWorldObjectGrowthServerRpc", &[json!(grown), json!(vegetable), json!([1, 4])]);
        assert_eq!(sent(&g, "WorldObjectsHandler.SetSceneWorldObjectGrowthClientRpc").iter().map(|s| (s.to, s.args.clone())).collect::<Vec<_>>(), vec![(Target::All, vec![json!(grown), json!(vegetable), json!([1, 4])])]);
        assert_eq!((g.wos[&grown].count, g.wos[&grown].growth), ([1, 4], 25.0));

        // A random-appearance object that appears (chance 100) is placed; one that does not
        // (chance 0) goes from the asker's scene.
        let (appears, absent, rot) = (SCENE + 3, SCENE + 4, [0.0, 0.0, 0.0, 1.0]);
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc", &[json!(100.0), json!(iron), json!(appears), json!(-1), json!([570.0, 3.0, 490.0]), json!(rot)]);
        send_scene(&mut g, 1, "WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc", &[json!(0.0), json!(iron), json!(absent), json!(-1), json!([571.0, 3.0, 490.0]), json!(rot)]);
        assert_eq!(g.wos[&appears].position, [570.0, 3.0, 490.0]);
        assert_eq!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").iter().map(|s| (s.to, s.args[0].clone())).collect::<Vec<_>>(), vec![(Target::Only(1), json!(absent))]);
        // Asked again from where it stands, it stays; from 2 m away (moved scene), it goes.
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc", &[json!(0.0), json!(iron), json!(appears), json!(-1), json!([570.0, 3.0, 490.0]), json!(rot)]);
        assert!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").is_empty());
        send_scene(&mut g, 1, "WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc", &[json!(100.0), json!(iron), json!(appears), json!(-1), json!([575.0, 3.0, 490.0]), json!(rot)]);
        assert_eq!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").len(), 1);

        // After a restart, a joining client asks about each scene object it has.
        let mut g = reload(&mut g);
        join(&mut g, 2, "Bob");
        g.net.out.clear();
        for id in [SCENE, disabled, 100_000_001] {
            send_scene(&mut g, 2, "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc", &[json!(id)]);
        }
        let gone: Vec<Value> = sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").into_iter().inspect(|s| assert_eq!(s.to, Target::Only(2))).map(|s| s.args[0].clone()).collect();
        assert_eq!(gone, vec![json!(SCENE), json!(disabled)], "the untouched one stays");
        send_scene(&mut g, 2, "WorldObjectsHandler.RefreshSceneWorldObjectGrowthServerRpc", &[json!(grown)]);
        assert_eq!(sent(&g, "WorldObjectsHandler.SetSceneWorldObjectGrowthClientRpc").last().unwrap().args, vec![json!(grown), json!(vegetable), json!([1, 4])]);
        assert_eq!(g.wos[&appears].position, [570.0, 3.0, 490.0]);
    }

    #[test]
    fn deconstruction_gives_the_recipe_to_the_backpack_or_the_floor() {
        let Some((mut g, _)) = world() else { return };
        let (inventory, _) = backpack(&g, 1);
        let generator = g.data.group("EnergyGenerator1").unwrap();
        let recipe: Vec<usize> = g.data.groups[generator].recipe.clone();
        assert!(!recipe.is_empty());
        let (built, _) = place(&mut g, "EnergyGenerator1", [600.0, 3.0, 500.0]);
        let group_hash = g.data.groups[generator].hash;
        let had = g.inventories[&inventory].items.len();
        g.net.out.clear();
        send_scene(&mut g, 1, "WorldObjectsHandler.RetrieveResourcesFromDeconstructionServerRpc", &[json!(built), json!(group_hash), json!(inventory), json!([600.0, 3.0, 500.0]), json!(1)]);
        let mut got: Vec<usize> = g.inventories[&inventory].items[had..].iter().map(|i| g.wos[i].group).collect();
        let mut want = recipe.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want);
        let hashes: Vec<Value> = recipe.iter().map(|r| json!(g.data.groups[*r].hash)).collect();
        assert_eq!(one(&g, "WorldObjectsHandler.DisplayDeconstructResultClientRpc"), vec![json!([]), json!(hashes)]);

        // No inventory (-1): everything drops a metre above the spot.
        let before = g.wos.len();
        send_scene(&mut g, 1, "WorldObjectsHandler.RetrieveResourcesFromDeconstructionServerRpc", &[json!(built), json!(group_hash), json!(-1), json!([600.0, 3.0, 500.0]), json!(1)]);
        let dropped: Vec<_> = g.wos.values().skip(before).collect();
        assert_eq!(dropped.len(), recipe.len());
        assert!(dropped.iter().all(|w| w.position == [600.0, 4.0, 500.0] && w.net.is_some()));

        // Free craft: deconstruction gives nothing back.
        g.save.settings["freeCraft"] = json!(true);
        let before = g.wos.len();
        send_scene(&mut g, 1, "WorldObjectsHandler.RetrieveResourcesFromDeconstructionServerRpc", &[json!(built), json!(group_hash), json!(inventory), json!([600.0, 3.0, 500.0]), json!(1)]);
        assert_eq!(g.wos.len(), before);
    }
}
