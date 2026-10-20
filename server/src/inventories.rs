//! SpaceCraft.InventoriesHandler and Inventory, server side: every inventory's content, the
//! clients watching an open inventory, and the add/remove/transfer requests with their replies
//! (each request gets exactly one reply: clients match replies to requests by order).

use std::cmp::Ordering;

use serde_json::json;

use crate::game::{Args, Game};
use crate::model::Inventory;
use crate::net::Target;

/// Inventory ids at or above this are not counted when picking the next id.
pub const SPECIAL_INVENTORY_IDS: i32 = 9_999_999;
/// The game's inventories hold a few dozen; a client asking for billions makes machines that
/// fill them (animal shelters) run away, and a sum past i32 wraps to a negative size.
const MAX_INVENTORY_SIZE: i32 = 1000;

impl Game {
    // ---- Inventory (the model) ------------------------------------------------------------

    /// Inventory.GetIsAuthorized.
    fn is_authorized(&self, inv: &Inventory, wo: i32) -> bool {
        let Some(group) = self.wos.get(&wo).map(|w| w.group) else { return false };
        if inv.is_equipment {
            let g = &self.data.groups[group];
            if g.constructible {
                return false;
            }
            let ty = g.str("equipableType");
            if ty != "Null" && inv.items.iter().any(|i| self.wos.get(i).is_some_and(|w| self.data.groups[w.group].str("equipableType") == ty)) {
                return false;
            }
        }
        !((!inv.authorized.is_empty() && !inv.authorized.contains(&group)) || inv.unauthorized.contains(&group))
    }

    /// Inventory.AddItem.
    pub fn inventory_add(&mut self, inventory: i32, wo: i32, reset_position: bool) -> bool {
        let Some(inv) = self.inventories.get(&inventory) else { return false };
        if !self.is_authorized(inv, wo) || inv.is_full() {
            return false;
        }
        if inv.is_dna {
            let t = self.wos.get(&wo).map(|w| w.trait_type).unwrap_or(0);
            if inv.items.iter().any(|i| self.wos.get(i).is_some_and(|w| w.trait_type == t)) {
                return false;
            }
        }
        let inv = self.inventories.get_mut(&inventory).unwrap();
        if !inv.items.contains(&wo) {
            inv.items.push(wo);
            self.hold(wo, inventory);
        }
        if let Some(w) = self.wos.get_mut(&wo) {
            if reset_position {
                w.reset_position();
            }
            w.lock_time = 0.0;
        }
        true
    }

    pub fn inventory_remove(&mut self, inventory: i32, wo: i32) -> bool {
        let Some(inv) = self.inventories.get_mut(&inventory) else { return false };
        match inv.items.iter().position(|i| *i == wo) {
            Some(at) => {
                inv.items.remove(at);
                self.release(wo);
                true
            }
            None => false,
        }
    }

    /// Inventory.RemoveItems(groups): the last item of each listed group.
    fn remove_items_of_groups(&mut self, inventory: i32, groups: &[usize]) -> Vec<i32> {
        let mut removed = Vec::new();
        for g in groups {
            let Some(inv) = self.inventories.get(&inventory) else { break };
            if let Some(at) = inv.items.iter().rposition(|i| self.wos.get(i).is_some_and(|w| w.group == *g)) {
                let wo = self.inventories.get_mut(&inventory).unwrap().items.remove(at);
                self.release(wo);
                removed.push(wo);
            }
        }
        removed
    }

    /// Inventory.RemoveItems(worldObjects): all or nothing.
    fn remove_items_by_id(&mut self, inventory: i32, ids: &[i32]) -> bool {
        let Some(inv) = self.inventories.get_mut(&inventory) else { return false };
        if !ids.iter().all(|i| inv.items.contains(i)) {
            return false;
        }
        let removed: Vec<i32> = inv.items.iter().copied().filter(|i| ids.contains(i)).collect();
        inv.items.retain(|i| !ids.contains(i));
        removed.into_iter().for_each(|wo| self.release(wo));
        true
    }

    /// Inventory.DestroyAllItemsInside: everything but what cannot be destroyed leaves the
    /// inventory (the caller tells the listeners, then destroys it).
    fn remove_all_items_inside(&mut self, inventory: i32) -> Vec<i32> {
        let Some(inv) = self.inventories.get(&inventory) else { return Vec::new() };
        let mut removed = Vec::new();
        for i in (0..inv.items.len()).rev() {
            let wo = self.inventories[&inventory].items[i];
            let keep = self.wos.get(&wo).is_some_and(|w| !self.data.groups[w.group].constructible && self.data.groups[w.group].bool("cantBeDestroyed"));
            if !keep {
                self.inventories.get_mut(&inventory).unwrap().items.remove(i);
                self.release(wo);
                removed.push(wo);
            }
        }
        removed
    }

    /// Inventory.PropagateModification: what listens to inventory changes on the server.
    pub fn propagate(&mut self, inventory: i32, wo: i32, added: bool) {
        crate::players::inventory_changed(self, inventory, wo, added);
        crate::hooks::inventory_changed(self, inventory, wo, added);
    }

    fn sort_inventory(&mut self, inventory: i32) {
        let Some(mut items) = self.inventories.get(&inventory).map(|i| i.items.clone()) else { return };
        // Inventory.AutoSort: List<WorldObject>.Sort(SortInventory), an introsort: items that
        // compare equal (the same group, or the same group, trait and colour) end in its order.
        // SortInventory is no order when one item has a trait and the other has none; the game's
        // sort lives with that, and so does this one (an error where the game throws).
        let _ = crate::dotnet_sort::sort_by(&mut items, |a, b| {
            let (Some(x), Some(y)) = (self.wos.get(a), self.wos.get(b)) else { return Ordering::Equal };
            culture_cmp(&self.data.groups[x.group].id, &self.data.groups[y.group].id).then_with(|| trait_order((x.trait_type, x.trait_value, x.color), (y.trait_type, y.trait_value, y.color)))
        });
        self.inventories.get_mut(&inventory).unwrap().items = items;
    }

    // ---- which inventory holds a world object ---------------------------------------------

    /// After `wo` went into `inventory`'s items.
    fn hold(&mut self, wo: i32, inventory: i32) {
        self.holders.entry(wo).and_modify(|h| h.1 += 1).or_insert((inventory, 1));
    }

    /// After one of `wo`'s places in an inventory's items went.
    pub(crate) fn release(&mut self, wo: i32) {
        let Some(h) = self.holders.get_mut(&wo) else { return };
        h.1 -= 1;
        match h.1 {
            0 => {
                self.holders.remove(&wo);
            }
            // Held twice (a save can say so), now once: by which one is looked up (rare).
            1 => {
                let left = self.inventories.values().find(|i| i.items.contains(&wo)).map(|i| i.id);
                match left {
                    Some(inv) => self.holders.insert(wo, (inv, 1)),
                    None => self.holders.remove(&wo),
                };
            }
            _ => {}
        }
    }

    /// The inventory holding a world object; the first of them, as the game looks it up, when
    /// several do.
    pub fn holder(&self, wo: i32) -> Option<i32> {
        match self.holders.get(&wo)? {
            (inv, 1) => Some(*inv),
            _ => self.inventories.values().find(|i| i.items.contains(&wo)).map(|i| i.id),
        }
    }

    pub fn is_held(&self, wo: i32) -> bool {
        self.holders.contains_key(&wo)
    }

    /// The index from the inventories (after a load or a restore).
    pub fn rebuild_holders(&mut self) {
        self.holders.clear();
        let all: Vec<(i32, i32)> = self.inventories.values().flat_map(|i| i.items.iter().map(move |wo| (*wo, i.id))).collect();
        for (wo, inv) in all {
            self.hold(wo, inv);
        }
    }

    // ---- players' inventories --------------------------------------------------------------

    /// The player whose backpack or equipment an inventory is, in the game or saved.
    pub fn inventory_player(&self, inventory: i32) -> Option<u64> {
        let ready = self.clients.values().filter_map(|c| c.ready.as_ref()).find(|s| s.inventory == inventory || s.equipment == inventory).map(|s| s.id);
        ready.or_else(|| {
            let mine = |p: &&serde_json::Value| ["inventoryId", "equipmentId"].iter().any(|k| p[*k].as_i64() == Some(inventory as i64));
            self.saved_players.iter().find(mine).map(|p| p["id"].as_u64().unwrap_or(0))
        })
    }

    /// Whether a client's change would touch another player's backpack or equipment (logged
    /// once per client). Nothing in the game has one client change another's: death drops,
    /// deconstruction and petting rewards are the server's own, on the player concerned.
    pub fn others_inventory(&mut self, client: u64, inventory: i32) -> bool {
        let Some(owner) = self.inventory_player(inventory) else { return false };
        if self.clients.get(&client).and_then(|c| c.ready.as_ref()).is_some_and(|s| s.id == owner) {
            return false;
        }
        if self.refused_inventories.insert(client) {
            eprintln!("client {client}: a change to another player's inventory {inventory} refused (logged once per client)");
        }
        true
    }

    /// Whether a world object is in another player's backpack or equipment.
    pub fn others_item(&mut self, client: u64, wo: i32) -> bool {
        self.holder(wo).is_some_and(|inv| self.others_inventory(client, inv))
    }

    // ---- InventoriesHandler ---------------------------------------------------------------

    fn biggest_inventory_id(&self) -> i32 {
        self.inventories.values().map(|i| i.id).filter(|id| *id < SPECIAL_INVENTORY_IDS).max().unwrap_or(0).max(0)
    }

    /// The next id after the biggest, as the game does; the lowest free one once that reaches
    /// the special ids. Never a player's (one whose inventory went would get a stranger's).
    fn free_inventory_id(&self) -> i32 {
        let free = |id: &i32| !self.inventories.contains_key(id) && self.inventory_player(*id).is_none();
        let next = self.biggest_inventory_id() + 1;
        if next < SPECIAL_INVENTORY_IDS && free(&next) {
            return next;
        }
        (1..SPECIAL_INVENTORY_IDS).find(free).unwrap_or(next)
    }

    /// The create path of GetOrCreateNewInventory; returns the inventory id.
    pub fn create_inventory(&mut self, size: i32, id: i32, owner: i32, content: &[usize], authorized: &[usize]) -> i32 {
        if let Some(existing) = self.inventories.get(&id).filter(|_| id != 0 && id != -1) {
            return existing.id;
        }
        let id = match id {
            -1 => {
                self.tmp_inventory_id -= 1;
                self.tmp_inventory_id + 1
            }
            // Ids a client picks stay below the special ones and never replace an inventory.
            other if other > 0 && other < SPECIAL_INVENTORY_IDS && !self.inventories.contains_key(&other) && self.inventory_player(other).is_none() => other,
            _ => self.free_inventory_id(),
        };
        self.inventories.insert(id, Inventory::new(id, size));
        // No more initial items than the inventory holds.
        for g in content.iter().take(size.max(0) as usize) {
            let wo = self.create_world_object(*g, 0, true);
            self.inventory_add(id, wo, true);
        }
        if owner > 0 {
            if let Some(w) = self.wos.get_mut(&owner) {
                w.linked_inventory = id;
            }
        }
        if !authorized.is_empty() {
            self.set_authorized(id, authorized.to_vec());
        }
        id
    }

    fn set_authorized(&mut self, inventory: i32, groups: Vec<usize>) {
        let equipment = groups.first().is_some_and(|g| !self.data.groups[*g].constructible && self.data.groups[*g].str("itemCategory") == "Equipment");
        if let Some(inv) = self.inventories.get_mut(&inventory) {
            inv.authorized = groups;
            if equipment {
                inv.is_equipment = true;
            }
        }
    }

    fn retrieve_inventory(&mut self, to: Target, inventory: Option<i32>) {
        let reply = match inventory.and_then(|id| self.inventories.get(&id)) {
            Some(inv) => {
                let groups: Vec<i32> = inv.items.iter().map(|i| self.wos.get(i).map(|w| self.data.groups[w.group].hash).unwrap_or(0)).collect();
                let traits = inv.items.iter().any(|i| self.wos.get(i).is_some_and(|w| w.trait_type != 0));
                json!([inv.size, inv.id, groups, inv.items, traits])
            }
            None => json!([0, -1, [], [], false]),
        };
        self.net.rpc_scene(to, "InventoriesHandler.RetrieveInventoryClientRpc", reply.as_array().unwrap());
    }

    #[allow(clippy::too_many_arguments)]
    fn get_or_create(&mut self, client: u64, create: bool, size: i32, id: i32, owner: i32, content: Option<Vec<i32>>, authorized: Option<Vec<i32>>) {
        let mut inventory = self.inventories.contains_key(&id).then_some(id);
        if create && inventory.is_none() {
            let groups: Vec<usize> = content.unwrap_or_default().iter().filter_map(|h| self.data.group_by_hash(*h)).collect();
            inventory = Some(self.create_inventory(size, id, owner, &groups, &[]));
        }
        if let (Some(inv), Some(auth)) = (inventory, authorized) {
            if self.inventories[&inv].authorized.is_empty() && !auth.is_empty() {
                let groups = auth.iter().filter_map(|h| self.data.group_by_hash(*h)).collect();
                self.set_authorized(inv, groups);
            }
        }
        self.retrieve_inventory(Target::Only(client), inventory);
    }

    /// InventoriesHandler.DestroyInventory; never a player's backpack or equipment (an object
    /// linked to one, GenerateRandomAppearanceObjectServerRpc, took it with it).
    pub fn destroy_inventory(&mut self, id: i32, destroy_go: bool) {
        if self.inventory_player(id).is_some() {
            return;
        }
        let Some(inv) = self.inventories.remove(&id) else { return };
        inv.items.iter().for_each(|wo| self.release(*wo));
        self.destroy_world_objects(&inv.items, destroy_go);
        crate::logistics::remove_inventory(self, id);
        self.close_inventory_on_watchers(id);
    }

    /// RemoveFromInventoryWhenDeleted: a destroyed world object leaves the inventory holding it.
    pub fn remove_from_inventory_when_deleted(&mut self, wo: i32) {
        if let Some(inv) = self.holder(wo) {
            self.inventory_remove(inv, wo);
            self.propagate(inv, wo, false);
            self.dirty_inventory(inv, 0, false);
        }
    }

    fn add_or_remove_result(&mut self, client: u64, ok: bool) {
        self.net.rpc_scene(Target::Only(client), "InventoriesHandler.AddOrRemoveItemClientRpc", &[json!(ok)]);
    }

    /// AddExistingItemToInventoryServerRpc without its reply. The game leaves the item where it
    /// was as well (a client removes it first), so two inventories would hold it and a save
    /// keep both; here it leaves its old inventory once the new one took it, as a removal does.
    pub fn add_existing_item(&mut self, wo: i32, inventory: i32, reset: bool, client: u64) -> bool {
        let old = self.holder(wo).filter(|h| *h != inventory);
        let ok = self.wos.contains_key(&wo) && self.inventory_add(inventory, wo, reset);
        if ok {
            self.propagate(inventory, wo, true);
            self.dirty_inventory(inventory, client, false);
            if let Some(old) = old.filter(|old| self.inventory_remove(*old, wo)) {
                self.propagate(old, wo, false);
                self.dirty_inventory(old, client, false);
            }
        }
        ok
    }

    /// AddNewItemToInventoryServerRpc without its reply: (success, world object id). The game
    /// keeps the new world object when the inventory refuses it (full, not authorized): a saved
    /// object in no inventory, one per call. Here it goes again.
    pub fn add_new_item(&mut self, group: usize, inventory: i32, client: u64) -> (bool, i32) {
        if !self.inventories.contains_key(&inventory) {
            return (false, 0);
        }
        let wo = self.create_world_object(group, 0, true);
        let ok = self.inventory_add(inventory, wo, true);
        if ok {
            self.propagate(inventory, wo, true);
            self.dirty_inventory(inventory, client, false);
        } else {
            self.destroy_world_object(wo, true);
        }
        (ok, wo)
    }

    /// DirtyInventory: the new content to every other client watching it.
    pub fn dirty_inventory(&mut self, inventory: i32, editing: u64, ignore_editing: bool) {
        let Some(watchers) = self.watchers.get(&inventory) else { return };
        if !ignore_editing && watchers.len() <= 1 && (watchers.len() != 1 || watchers.contains_key(&editing)) {
            return;
        }
        let targets: Vec<u64> = watchers.keys().copied().filter(|c| ignore_editing || *c != editing).collect();
        let Some(inv) = self.inventories.get(&inventory) else { return };
        let groups: Vec<i32> = inv.items.iter().map(|i| self.wos.get(i).map(|w| self.data.groups[w.group].hash).unwrap_or(0)).collect();
        let traits = inv.items.iter().any(|i| self.wos.get(i).is_some_and(|w| w.trait_type != 0));
        let args = [json!(inv.size), json!(inventory), json!(groups), json!(inv.items), json!(traits)];
        for c in targets {
            self.net.rpc_scene(Target::Only(c), "InventoriesHandler.DirtyInventoryClientRpc", &args);
        }
    }

    fn dirty_inventory_logistics(&mut self, inventory: i32, editing: u64, ignore_editing: bool) {
        let Some(watchers) = self.watchers.get(&inventory) else { return };
        if !ignore_editing && watchers.len() <= 1 && (watchers.len() != 1 || watchers.contains_key(&editing)) {
            return;
        }
        let targets: Vec<u64> = watchers.keys().copied().filter(|c| ignore_editing || *c != editing).collect();
        let (demand, supply, priority) = self.logistic_serialized(inventory);
        for c in targets {
            self.net.rpc_scene(Target::Only(c), "InventoriesHandler.DirtyInventoryLogisticsClientRpc", &[json!(inventory), json!(demand), json!(supply), json!(priority)]);
        }
    }

    /// LogisticEntity.Serialize: (demand hashes, supply hashes, priority).
    fn logistic_serialized(&self, inventory: i32) -> (Vec<i32>, Vec<i32>, i32) {
        let Some(inv) = self.inventories.get(&inventory) else { return (Vec::new(), Vec::new(), 0) };
        let hashes = |g: &[usize]| g.iter().map(|g| self.data.groups[*g].hash).collect::<Vec<_>>();
        (hashes(&inv.logistic.demand), hashes(&inv.logistic.supply), inv.logistic.priority)
    }

    pub fn stop_inventory_watch(&mut self, inventory: i32, client: u64) {
        if let Some(w) = self.watchers.get_mut(&inventory) {
            if let Some(count) = w.get_mut(&client) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    w.shift_remove(&client);
                }
            }
            if w.is_empty() {
                self.watchers.remove(&inventory);
            }
        }
    }

    fn close_inventory_on_watchers(&mut self, inventory: i32) {
        let clients: Vec<u64> = self.watchers.get(&inventory).map(|w| w.keys().rev().copied().collect()).unwrap_or_default();
        for c in clients {
            self.net.rpc_scene(Target::Only(c), "InventoriesHandler.CloseInventoryOnWatchersClientRpc", &[]);
        }
    }

    fn transfer(&mut self, from: i32, to: i32, items: Vec<i32>, editing: Option<u64>) -> bool {
        // Added where it already is, then removed: the game loses the item from the inventory.
        if from == to {
            return false;
        }
        let time = self.time;
        let mut moved = Vec::new();
        for wo in items {
            if self.wos.get(&wo).is_some_and(|w| w.is_locked_in_inventory(time)) || !self.inventories.get(&from).is_some_and(|i| i.items.contains(&wo)) {
                continue;
            }
            if self.inventory_add(to, wo, true) {
                self.inventory_remove(from, wo);
                moved.push(wo);
            }
        }
        if moved.is_empty() {
            return false;
        }
        for wo in &moved {
            self.propagate(from, *wo, false);
            self.propagate(to, *wo, true);
        }
        match editing {
            Some(c) => {
                self.dirty_inventory(from, c, false);
                self.dirty_inventory(to, c, false);
            }
            None => {
                self.dirty_inventory(from, 0, true);
                self.dirty_inventory(to, 0, true);
            }
        }
        true
    }

    /// TransferItem called by the server itself.
    pub fn transfer_one(&mut self, from: i32, to: i32, wo: i32, editor: u64) -> bool {
        self.transfer(from, to, vec![wo], Some(editor))
    }

    /// The InventoriesHandler RPCs; false when `name` is not one of them.
    pub fn inventories_rpc(&mut self, client: u64, name: &str, a: &Args) -> bool {
        // What a change touches: another player's backpack or equipment, or an item in one, is
        // refused (the game's host takes these from anyone).
        let (touched, items): (Vec<i32>, Vec<i32>) = match name {
            "InventoriesHandler.TransferItemServerRpc" | "InventoriesHandler.TransferAllSameGroupServerRpc" | "InventoriesHandler.TransferAllItemsServerRpc" => (vec![a.i(0), a.i(1)], vec![]),
            "InventoriesHandler.AddNewItemToInventoryServerRpc" | "InventoriesHandler.RemoveItemsFromInventoryServerRpc" => (vec![a.i(1)], vec![]),
            "InventoriesHandler.GetOrCreateNewInventoryServerRpc" => (vec![a.i(2)], vec![]),
            "InventoriesHandler.AddExistingItemToInventoryServerRpc" | "InventoriesHandler.RemoveItemFromInventoryServerRpc" => (vec![a.i(1)], vec![a.i(0)]),
            "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc" => (vec![a.i(1)], a.ints(0).unwrap_or_default()),
            "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc"
            | "InventoriesHandler.DirtyInventoryLogisticsServerRpc"
            | "InventoriesHandler.SetInventorySizeServerRpc"
            | "InventoriesHandler.LockInventoryContentServerRpc"
            | "InventoriesHandler.SortInventoryServerRpc" => (vec![a.i(0)], vec![]),
            "InventoriesHandler.AddWorldObjectToInventoryServerRpc" => (vec![a.i(2)], vec![a.i(0)]),
            _ => (vec![], vec![]),
        };
        if touched.iter().any(|i| self.others_inventory(client, *i)) || items.iter().any(|w| self.others_item(client, *w)) {
            match name {
                "InventoriesHandler.AddNewItemToInventoryServerRpc" => self.net.rpc_scene(Target::Only(client), "InventoriesHandler.AddNewItemClientRpc", &[json!(false), json!(0)]),
                // Neither its content nor a change of what it takes: an empty inventory.
                "InventoriesHandler.GetOrCreateNewInventoryServerRpc" => self.retrieve_inventory(Target::Only(client), None),
                "InventoriesHandler.DirtyInventoryLogisticsServerRpc" | "InventoriesHandler.SortInventoryServerRpc" => {}
                _ => self.add_or_remove_result(client, false),
            }
            return true;
        }
        match name {
            "InventoriesHandler.GetOrCreateNewInventoryFromWoIDServerRpc" => {
                match self.wos.get(&a.i(0)).map(|w| w.linked_inventory) {
                    Some(inv) => self.get_or_create(client, false, 0, inv, 0, None, None),
                    None => self.retrieve_inventory(Target::Only(client), None),
                }
            }
            "InventoriesHandler.GetOrCreateNewInventoryServerRpc" => {
                self.get_or_create(client, a.b(0), a.i(1).clamp(0, MAX_INVENTORY_SIZE), a.i(2), a.i(3), a.ints(4), a.ints(5));
            }
            "InventoriesHandler.TransferItemServerRpc" => {
                let ok = self.transfer(a.i(0), a.i(1), vec![a.i(2)], Some(client));
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.TransferAllSameGroupServerRpc" => {
                let (from, to, hash) = (a.i(0), a.i(1), a.i(2));
                let items: Vec<i32> = self.inventories.get(&from).map(|i| i.items.iter().rev().copied().filter(|w| self.wos.get(w).is_some_and(|w| self.data.groups[w.group].hash == hash)).collect()).unwrap_or_default();
                let ok = self.inventories.contains_key(&to) && self.transfer(from, to, items, None);
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.TransferAllItemsServerRpc" => {
                let (from, to) = (a.i(0), a.i(1));
                let items: Vec<i32> = self.inventories.get(&from).map(|i| i.items.iter().rev().copied().collect()).unwrap_or_default();
                let ok = self.transfer(from, to, items, None);
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.AddNewItemToInventoryServerRpc" => {
                let (ok, wo) = match self.data.group_by_hash(a.i(0)) {
                    Some(g) => self.add_new_item(g, a.i(1), client),
                    None => (false, 0),
                };
                self.net.rpc_scene(Target::Only(client), "InventoriesHandler.AddNewItemClientRpc", &[json!(ok), json!(wo)]);
            }
            "InventoriesHandler.AddExistingItemToInventoryServerRpc" => {
                let ok = self.add_existing_item(a.i(0), a.i(1), a.b(2), client);
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.RemoveItemFromInventoryServerRpc" => {
                let (wo, inv, destroy) = (a.i(0), a.i(1), a.b(2));
                let ok = self.wos.contains_key(&wo) && self.inventory_remove(inv, wo);
                if ok {
                    // The listeners read the item (its group) as it leaves: before it is destroyed.
                    self.propagate(inv, wo, false);
                    if destroy {
                        self.destroy_world_object(wo, false);
                    }
                    self.dirty_inventory(inv, client, false);
                }
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.RemoveItemsFromInventoryServerRpc" => {
                let (hashes, inv, destroy) = (a.ints(0).unwrap_or_default(), a.i(1), a.b(2));
                let ok = self.inventories.contains_key(&inv);
                if ok {
                    let groups: Vec<usize> = hashes.iter().filter_map(|h| self.data.group_by_hash(*h)).collect();
                    let removed = self.remove_items_of_groups(inv, &groups);
                    for wo in &removed {
                        self.propagate(inv, *wo, false);
                    }
                    if destroy {
                        self.destroy_world_objects(&removed, false);
                    }
                    self.dirty_inventory(inv, client, false);
                }
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc" => {
                let (ids, inv, destroy) = (a.ints(0).unwrap_or_default(), a.i(1), a.b(2));
                let ok = self.remove_items_by_id(inv, &ids);
                if ok {
                    for wo in &ids {
                        self.propagate(inv, *wo, false);
                    }
                    if destroy {
                        self.destroy_world_objects(&ids, false);
                    }
                    self.dirty_inventory(inv, client, false);
                }
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc" => {
                let inv = a.i(0);
                let ok = self.inventories.contains_key(&inv);
                if ok {
                    let removed = self.remove_all_items_inside(inv);
                    for wo in &removed {
                        self.propagate(inv, *wo, false);
                    }
                    self.destroy_world_objects(&removed, false);
                    self.dirty_inventory(inv, client, false);
                }
                self.add_or_remove_result(client, ok);
            }
            "InventoriesHandler.RetrieveInventoryLogisticsServerRpc" => {
                let inv = a.i(0);
                let (demand, supply, priority) = self.logistic_serialized(inv);
                self.net.rpc_scene(Target::Only(client), "InventoriesHandler.DirtyInventoryLogisticsClientRpc", &[json!(inv), json!(demand), json!(supply), json!(priority)]);
            }
            "InventoriesHandler.DirtyInventoryLogisticsServerRpc" if !self.inventories.contains_key(&a.i(0)) => {}
            "InventoriesHandler.DirtyInventoryLogisticsServerRpc" => {
                let inv = a.i(0);
                let groups = |v: Option<Vec<i32>>| v.unwrap_or_default().iter().filter_map(|h| self.data.group_by_hash(*h)).collect::<Vec<_>>();
                let (demand, supply) = (groups(a.ints(1)), groups(a.ints(2)));
                if let Some(i) = self.inventories.get_mut(&inv) {
                    i.logistic.demand = demand;
                    i.logistic.supply = supply;
                    i.logistic.priority = a.i(3);
                }
                crate::logistics::set_dirty(self, inv);
                self.dirty_inventory_logistics(inv, client, false);
            }
            "InventoriesHandler.SetInventorySizeServerRpc" => {
                let (inv, offset, drop) = (a.i(0), a.i(1), a.v3(2));
                if let Some(size) = self.inventories.get(&inv).map(|i| (i.size as i64 + offset as i64).clamp(0, MAX_INVENTORY_SIZE as i64) as i32) {
                    self.inventories.get_mut(&inv).unwrap().size = size;
                    // DropObjectsIfNotEnoughSpace.
                    let overflow: Vec<i32> = self.inventories[&inv].items.iter().skip(size.max(0) as usize).copied().collect();
                    if self.inventories[&inv].is_full() {
                        for wo in overflow.iter().rev() {
                            // Inventory.DropObjectsIfNotEnoughSpace calls DropOnFloor as the server.
                            self.drop_on_floor(*wo, drop, [0.0, 0.0, 0.0, 1.0], 0.0, true, false, 0, 0.0, false, 0);
                            self.inventory_remove(inv, *wo);
                            self.propagate(inv, *wo, false);
                        }
                    }
                    self.dirty_inventory(inv, client, false);
                }
                self.add_or_remove_result(client, true);
            }
            "InventoriesHandler.LockInventoryContentServerRpc" => {
                let (inv, locked, lock_time) = (a.i(0), a.b(1), a.f(2));
                let until = if lock_time < f32::EPSILON { self.time - 0.01 } else { self.time + lock_time };
                let time = self.time;
                for wo in self.inventories.get(&inv).map(|i| i.items.clone()).unwrap_or_default() {
                    if let Some(w) = self.wos.get_mut(&wo) {
                        if !locked || !w.is_locked_in_inventory(time) {
                            w.lock_time = if locked { until } else { 0.0 };
                        }
                    }
                }
                self.add_or_remove_result(client, true);
            }
            "InventoriesHandler.SortInventoryServerRpc" => self.sort_inventory(a.i(0)),
            "InventoriesHandler.GetInventoryFromFirstWorldObjectOfGroupServerRpc" => {
                let found = self.data.group_by_hash(a.i(0)).and_then(|g| self.first_world_object_of_group(g, a.i(1)));
                match found.map(|w| self.wos[&w].linked_inventory) {
                    Some(inv) => self.get_or_create(client, false, 0, inv, 0, None, None),
                    None => self.retrieve_inventory(Target::Only(client), None),
                }
            }
            // Watches, logistics settings: of inventories that exist only (the game keeps an
            // entry for any id a client sends, so the server's memory grew without bound).
            "InventoriesHandler.BeginInventoryWatchServerRpc" if !self.inventories.contains_key(&a.i(0)) => {}
            "InventoriesHandler.BeginInventoryWatchServerRpc" => {
                *self.watchers.entry(a.i(0)).or_default().entry(client).or_insert(0) += 1;
            }
            "InventoriesHandler.StopInventoryWatchServerRpc" => self.stop_inventory_watch(a.i(0), client),
            "InventoriesHandler.CheckInventoryWatchAndDirtyServerRpc" if !self.inventories.contains_key(&a.i(0)) => {}
            "InventoriesHandler.CheckInventoryWatchAndDirtyServerRpc" => {
                let inv = a.i(0);
                let w = self.watchers.entry(inv).or_default();
                if w.get(&client).copied().unwrap_or(0) == 0 {
                    w.insert(client, 1);
                }
                self.dirty_inventory(inv, 0, true);
            }
            "InventoriesHandler.AddWorldObjectToInventoryServerRpc" => {
                let (id, hash, inv, grabbed) = (a.i(0), a.i(1), a.i(2), a.b(3));
                let mut created = false;
                // Scene world objects have positive ids; 0 would make a fresh one.
                if id > 0 && !self.wos.contains_key(&id) {
                    if let Some(g) = self.data.group_by_hash(hash) {
                        self.create_world_object(g, id, false);
                        created = true;
                    }
                }
                let placed = self.wos.get(&id).is_some_and(|w| w.is_placed());
                // An item dropped without leaving its inventory is in two once picked up.
                let old = self.holder(id).filter(|h| *h != inv);
                if (placed || created) && self.inventories.contains_key(&inv) && self.inventory_add(inv, id, true) {
                    self.propagate(inv, id, true);
                    if let Some(old) = old.filter(|old| self.inventory_remove(*old, id)) {
                        self.propagate(old, id, false);
                        self.dirty_inventory(old, client, false);
                    }
                    // The inventory's machine may have destroyed it already
                    // (MachineDestructInventoryIfFull).
                    if let Some(was_unsaved) = self.wos.get(&id).map(|w| w.dont_save) {
                        self.wos.get_mut(&id).unwrap().dont_save = false;
                        self.update_save_state(id);
                        if was_unsaved {
                            crate::scene::on_world_object_from_scene_retrieved(self, id);
                        }
                        if grabbed {
                            crate::machines::grabbed(self, id);
                        }
                        self.destroy_go_on_all_clients(id, 0.0);
                    }
                    self.dirty_inventory(inv, client, false);
                    self.add_or_remove_result(client, true);
                } else {
                    self.add_or_remove_result(client, false);
                }
            }
            _ => return false,
        }
        true
    }
}

/// SortInventory's order of two items of one group: an item without a trait (type 0) is equal to
/// every other (it is not looked at further), one with a trait goes by type, value and the text
/// of its color (Color.ToString(): three decimals). So x with a trait comes after y without one,
/// but y without one is equal to x: no order, which only the game's own sort gives the answer to.
fn trait_order(x: (i32, i32, [f32; 4]), y: (i32, i32, [f32; 4])) -> Ordering {
    let text = |c: [f32; 4]| format!("RGBA({:.3}, {:.3}, {:.3}, {:.3})", c[0], c[1], c[2], c[3]);
    match x.0 {
        0 => Ordering::Equal,
        _ => x.0.cmp(&y.0).then(x.1.cmp(&y.1)).then_with(|| culture_cmp(&text(x.2), &text(y.2))),
    }
}

/// .NET culture-aware string order for ASCII ids: letters compare case-insensitively first,
/// then lower case before upper case.
pub fn culture_cmp(a: &str, b: &str) -> Ordering {
    let fold = |s: &str| s.chars().map(|c| c.to_ascii_lowercase()).collect::<String>();
    fold(a).cmp(&fold(b)).then_with(|| {
        for (x, y) in a.chars().zip(b.chars()) {
            if x != y {
                return if x.is_ascii_lowercase() { Ordering::Less } else { Ordering::Greater };
            }
        }
        a.len().cmp(&b.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;
    use serde_json::Value;

    #[test]
    fn items_with_and_without_traits_sort_as_the_games_sort_does() {
        // SortInventory is no order (a trait-less item is equal to all): the game's sort takes
        // what comes, and ends; this one must not panic on it.
        let mut seed = 7u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as i32
        };
        for n in [5, 21, 22, 40, 100, 400] {
            for _ in 0..200 {
                let mut items: Vec<(i32, i32, [f32; 4])> = (0..n).map(|_| (if next() % 2 == 0 { 0 } else { next() % 9 }, next() % 5, [(next() % 3) as f32, 0.0, 0.0, 1.0])).collect();
                let _ = crate::dotnet_sort::sort_by(&mut items, |x, y| trait_order(*x, *y));
            }
        }
        assert_eq!(trait_order((0, 5, [0.0; 4]), (0, 9, [1.0; 4])), Ordering::Equal, "no trait: equal");
        assert_eq!(trait_order((0, 5, [0.0; 4]), (2, 9, [1.0; 4])), Ordering::Equal, "x without a trait is equal to all");
        assert_eq!(trait_order((2, 9, [1.0; 4]), (0, 5, [0.0; 4])), Ordering::Greater, "but one with a trait is after it");
        assert_eq!(trait_order((2, 1, [0.0; 4]), (2, 3, [0.0; 4])), Ordering::Less);
        // The color goes by its text with three decimals: colors that round alike are equal.
        assert_eq!(trait_order((2, 1, [0.1, 0.0, 0.0, 1.0]), (2, 1, [0.1004, 0.0, 0.0, 1.0])), Ordering::Equal);
        assert_eq!(trait_order((2, 1, [0.1, 0.0, 0.0, 1.0]), (2, 1, [0.2, 0.0, 0.0, 1.0])), Ordering::Less);
    }

    #[test]
    fn sorting_an_inventory_of_many_equal_items_is_the_games_introsort() {
        let Some((mut g, _)) = world() else { return };
        let chest = g.create_inventory(30, 0, 0, &[], &[]);
        let items: Vec<i32> = (0..20).map(|_| give(&mut g, "Iron", chest)).collect();
        send_scene(&mut g, 1, "InventoriesHandler.SortInventoryServerRpc", &[json!(chest)]);
        let mut want = items.clone();
        crate::dotnet_sort::sort_by(&mut want, |_, _| Ordering::Equal).unwrap();
        assert_ne!(want, items, "20 equal items are moved by the introsort");
        assert_eq!(g.inventories[&chest].items, want);
    }

    /// The one AddOrRemoveItemClientRpc result each request gets.
    fn result(g: &mut Game) -> bool {
        let r = one(g, "InventoriesHandler.AddOrRemoveItemClientRpc")[0].as_bool().unwrap();
        g.net.out.clear();
        r
    }

    fn retrieved(g: &mut Game) -> Vec<Value> {
        let r = one(g, "InventoriesHandler.RetrieveInventoryClientRpc");
        g.net.out.clear();
        r
    }

    fn saved_items(g: &mut Game, inventory: i32) -> String {
        g.snapshot().inventories.iter().find(|i| i["id"] == inventory).map(|i| i["woIds"].as_str().unwrap().to_string()).unwrap()
    }

    #[test]
    fn inventories_are_created_and_found_as_the_game_asks() {
        let Some((mut g, _)) = world() else { return };
        let (iron, ice) = (hash(&g, "Iron"), hash(&g, "ice"));
        // Created with content and the groups it takes; the new id is the next after the biggest.
        let next = g.inventories.keys().filter(|i| **i < SPECIAL_INVENTORY_IDS).max().unwrap() + 1;
        send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &[json!(true), json!(4), json!(0), json!(0), json!([iron, iron]), json!([iron])]);
        let r = retrieved(&mut g);
        assert_eq!((&r[0], &r[1], &r[2], &r[4]), (&json!(4), &json!(next), &json!([iron, iron]), &json!(false)));
        assert_eq!(g.inventories[&next].items.len(), 2);
        // Only Iron goes in.
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(ice), json!(next)]);
        assert_eq!(one(&g, "InventoriesHandler.AddNewItemClientRpc")[0], json!(false));
        g.net.out.clear();
        // Not created when asked only to get: an empty answer.
        send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &[json!(false), json!(4), json!(next + 100), json!(0), json!(null), json!(null)]);
        assert_eq!(retrieved(&mut g), vec![json!(0), json!(-1), json!([]), json!([]), json!(false)]);
        // By world object, and by the first world object of a group (any planet: 0).
        let (container, _) = place(&mut g, "Container1", [570.0, 3.0, 500.0]);
        let pod = g.wos[&container].linked_inventory;
        send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryFromWoIDServerRpc", &[json!(container)]);
        assert_eq!(retrieved(&mut g)[1], json!(pod));
        send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryFromWoIDServerRpc", &[json!(5)]);
        assert_eq!(retrieved(&mut g)[1], json!(-1));
        let pod_group = hash(&g, "Container1");
        send_scene(&mut g, 1, "InventoriesHandler.GetInventoryFromFirstWorldObjectOfGroupServerRpc", &[json!(pod_group), json!(0)]);
        assert_eq!(retrieved(&mut g)[1], json!(pod));
        send_scene(&mut g, 1, "InventoriesHandler.GetInventoryFromFirstWorldObjectOfGroupServerRpc", &[json!(pod_group), json!(1234)]);
        assert_eq!(retrieved(&mut g)[1], json!(-1), "none on that planet");
        // A traits flag for inventories holding genetic traits.
        let dna = g.create_inventory(5, 0, 0, &[], &[]);
        let t = give(&mut g, crate::genetics::TRAIT_GROUP, dna);
        g.wos.get_mut(&t).unwrap().trait_type = 2;
        send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &[json!(false), json!(0), json!(dna), json!(0), json!(null), json!(null)]);
        assert_eq!(retrieved(&mut g)[4], json!(true));
    }

    /// Every add, remove and transfer gets exactly one AddOrRemoveItem result, and the save holds
    /// what each one did.
    #[test]
    fn items_are_added_removed_and_transferred_with_one_result_each() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let chest = g.create_inventory(3, 0, 0, &[], &[]);
        let (iron, ice) = (hash(&g, "Iron"), hash(&g, "ice"));

        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(chest)]);
        let a = one(&g, "InventoriesHandler.AddNewItemClientRpc");
        assert_eq!(a[0], json!(true));
        let first = a[1].as_i64().unwrap() as i32;
        g.net.out.clear();
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(12345), json!(chest)]);
        assert_eq!(one(&g, "InventoriesHandler.AddNewItemClientRpc"), vec![json!(false), json!(0)]);
        g.net.out.clear();
        let loose = g.create_world_object(g.data.group("ice").unwrap(), 0, true);
        send_scene(&mut g, 1, "InventoriesHandler.AddExistingItemToInventoryServerRpc", &[json!(loose), json!(chest), json!(true)]);
        assert!(result(&mut g));
        send_scene(&mut g, 1, "InventoriesHandler.AddExistingItemToInventoryServerRpc", &[json!(-5), json!(chest), json!(true)]);
        assert!(!result(&mut g), "no such object");
        assert_eq!(saved_items(&mut g, chest), format!("{first},{loose}"));

        // One item, all of a group, everything.
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(chest), json!(bag), json!(first)]);
        assert!(result(&mut g));
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(chest), json!(bag), json!(first)]);
        assert!(!result(&mut g), "not in there any more");
        assert!(g.inventories[&bag].items.contains(&first));
        let bag_ice = g.inventories[&bag].items.iter().filter(|i| g.data.groups[g.wos[*i].group].hash == ice).count();
        send_scene(&mut g, 1, "InventoriesHandler.TransferAllSameGroupServerRpc", &[json!(chest), json!(bag), json!(ice)]);
        assert!(result(&mut g));
        assert_eq!(g.inventories[&bag].items.iter().filter(|i| g.data.groups[g.wos[*i].group].hash == ice).count(), bag_ice + 1);
        send_scene(&mut g, 1, "InventoriesHandler.TransferAllSameGroupServerRpc", &[json!(chest), json!(-77), json!(ice)]);
        assert!(!result(&mut g), "no target inventory");
        let carried = g.inventories[&bag].items.clone();
        send_scene(&mut g, 1, "InventoriesHandler.TransferAllItemsServerRpc", &[json!(bag), json!(chest)]);
        assert!(result(&mut g));
        assert_eq!(g.inventories[&chest].items.len(), 3, "as many as fit");
        assert_eq!(g.inventories[&bag].items.len(), carried.len() - 3);

        // Removing: one by id (destroyed), the last of each group, a list (all or nothing), all.
        let back = g.inventories[&chest].items.clone();
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(back[0]), json!(chest), json!(true)]);
        assert!(result(&mut g));
        assert!(!g.wos.contains_key(&back[0]));
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(back[0]), json!(chest), json!(true)]);
        assert!(!result(&mut g));
        send_scene(&mut g, 1, "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc", &[json!([back[1], 999]), json!(chest), json!(true)]);
        assert!(!result(&mut g), "999 is not in there: nothing goes");
        assert_eq!(g.inventories[&chest].items, back[1..]);
        send_scene(&mut g, 1, "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc", &[json!([back[1]]), json!(chest), json!(false)]);
        assert!(result(&mut g));
        assert!(g.wos.contains_key(&back[1]), "kept: destroy was false");
        let group = g.data.groups[g.wos[&back[2]].group].hash;
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemsFromInventoryServerRpc", &[json!([group]), json!(chest), json!(true)]);
        assert!(result(&mut g));
        assert!(g.inventories[&chest].items.is_empty() && !g.wos.contains_key(&back[2]));
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemsFromInventoryServerRpc", &[json!([group]), json!(-9), json!(true)]);
        assert!(!result(&mut g));
        // All, but what cannot be destroyed.
        let keep = give(&mut g, "WardenKey", chest);
        let gone = give(&mut g, "Iron", chest);
        send_scene(&mut g, 1, "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", &[json!(chest)]);
        assert!(result(&mut g));
        assert_eq!(g.inventories[&chest].items, vec![keep]);
        assert!(!g.wos.contains_key(&gone));
        send_scene(&mut g, 1, "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", &[json!(-9)]);
        assert!(!result(&mut g));
        assert_eq!(saved_items(&mut g, chest), keep.to_string());
    }

    /// A client's size change cannot overflow the size or grow it without bound.
    #[test]
    fn a_size_change_stays_within_the_bounds_of_an_inventory() {
        let Some((mut g, _)) = world() else { return };
        let chest = g.create_inventory(3, 0, 0, &[], &[]);
        let items: Vec<i32> = ["Iron", "ice"].iter().map(|n| give(&mut g, n, chest)).collect();
        send_scene(&mut g, 1, "InventoriesHandler.SetInventorySizeServerRpc", &[json!(chest), json!(i32::MAX), json!([570.0, 3.0, 500.0])]);
        assert!(result(&mut g));
        assert_eq!((g.inventories[&chest].size, g.inventories[&chest].items.clone()), (MAX_INVENTORY_SIZE, items.clone()));
        send_scene(&mut g, 1, "InventoriesHandler.SetInventorySizeServerRpc", &[json!(chest), json!(i32::MIN), json!([570.0, 3.0, 500.0])]);
        assert!(result(&mut g));
        assert_eq!(g.inventories[&chest].size, 0);
        assert!(g.inventories[&chest].items.is_empty() && items.iter().all(|i| g.wos[i].is_placed()), "all dropped, none lost");
    }

    /// An open inventory's other watchers hear each change; the one making it does not.
    #[test]
    fn watchers_hear_the_changes_others_make() {
        let Some((mut g, _)) = world() else { return };
        join(&mut g, 2, "Bob");
        let chest = g.create_inventory(5, 0, 0, &[], &[]);
        let iron = hash(&g, "Iron");
        let dirty = |g: &Game| sent(g, "InventoriesHandler.DirtyInventoryClientRpc").into_iter().map(|s| s.to).collect::<Vec<_>>();
        send_scene(&mut g, 1, "InventoriesHandler.BeginInventoryWatchServerRpc", &[json!(chest)]);
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(chest)]);
        assert!(dirty(&g).is_empty(), "alone: no one to tell");
        send_scene(&mut g, 2, "InventoriesHandler.BeginInventoryWatchServerRpc", &[json!(chest)]);
        g.net.out.clear();
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(chest)]);
        assert_eq!(dirty(&g), vec![Target::Only(2)]);
        let d = &sent(&g, "InventoriesHandler.DirtyInventoryClientRpc")[0].args;
        assert_eq!((&d[0], &d[1], &d[2]), (&json!(5), &json!(chest), &json!([iron, iron])));
        // Stopped watching: no more news; CheckInventoryWatchAndDirty watches again and tells
        // every watcher, the asker too.
        send_scene(&mut g, 2, "InventoriesHandler.StopInventoryWatchServerRpc", &[json!(chest)]);
        g.net.out.clear();
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(chest)]);
        assert!(dirty(&g).is_empty());
        send_scene(&mut g, 2, "InventoriesHandler.CheckInventoryWatchAndDirtyServerRpc", &[json!(chest)]);
        assert_eq!(dirty(&g), vec![Target::Only(1), Target::Only(2)]);
        // Destroyed while open: every watcher's window closes.
        g.net.out.clear();
        g.destroy_inventory(chest, true);
        assert_eq!(sent(&g, "InventoriesHandler.CloseInventoryOnWatchersClientRpc").into_iter().map(|s| s.to).collect::<Vec<_>>(), vec![Target::Only(2), Target::Only(1)]);
    }

    #[test]
    fn logistics_settings_are_kept_saved_and_told_to_watchers() {
        let Some((mut g, _)) = world() else { return };
        join(&mut g, 2, "Bob");
        let chest = g.create_inventory(5, 0, 0, &[], &[]);
        let (iron, ice) = (hash(&g, "Iron"), hash(&g, "ice"));
        send_scene(&mut g, 2, "InventoriesHandler.BeginInventoryWatchServerRpc", &[json!(chest)]);
        send_scene(&mut g, 1, "InventoriesHandler.BeginInventoryWatchServerRpc", &[json!(chest)]);
        send_scene(&mut g, 1, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &[json!(chest), json!([iron]), json!([ice]), json!(2)]);
        let s = sent(&g, "InventoriesHandler.DirtyInventoryLogisticsClientRpc");
        assert_eq!(s.iter().map(|s| (s.to, s.args.clone())).collect::<Vec<_>>(), vec![(Target::Only(2), vec![json!(chest), json!([iron]), json!([ice]), json!(2)])]);
        g.net.out.clear();
        send_scene(&mut g, 2, "InventoriesHandler.RetrieveInventoryLogisticsServerRpc", &[json!(chest)]);
        assert_eq!(one(&g, "InventoriesHandler.DirtyInventoryLogisticsClientRpc"), vec![json!(chest), json!([iron]), json!([ice]), json!(2)]);
        let saved = g.snapshot().inventories.into_iter().find(|i| i["id"] == chest).unwrap();
        assert_eq!((&saved["demandGrps"], &saved["supplyGrps"], &saved["priority"]), (&json!("Iron"), &json!("ice"), &json!(2)));
    }

    #[test]
    fn shrinking_drops_what_no_longer_fits_and_locks_and_sorting_follow_the_game() {
        let Some((mut g, _)) = world() else { return };
        let chest = g.create_inventory(3, 0, 0, &[], &[]);
        let items: Vec<i32> = ["Iron", "ice", "Iron"].iter().map(|n| give(&mut g, n, chest)).collect();
        send_scene(&mut g, 1, "InventoriesHandler.SetInventorySizeServerRpc", &[json!(chest), json!(-1), json!([570.0, 3.0, 500.0])]);
        assert!(result(&mut g));
        assert_eq!((g.inventories[&chest].size, g.inventories[&chest].items.clone()), (2, items[..2].to_vec()));
        assert_eq!(g.wos[&items[2]].position, [570.0, 3.0, 500.0]);
        assert!(g.wos[&items[2]].net.is_some(), "on the floor");

        // Sorted by group id, case-insensitively: ice before Iron.
        g.inventories.get_mut(&chest).unwrap().size = 5;
        give(&mut g, "Iron", chest);
        send_scene(&mut g, 1, "InventoriesHandler.SortInventoryServerRpc", &[json!(chest)]);
        let names: Vec<&str> = g.inventories[&chest].items.iter().map(|i| g.data.groups[g.wos[i].group].id.as_str()).collect();
        assert_eq!(names, ["ice", "Iron", "Iron"]);

        // Locked for 2 s: the items lock once the time is up, and cannot be taken then.
        let (bag, _) = backpack(&g, 1);
        send_scene(&mut g, 1, "InventoriesHandler.LockInventoryContentServerRpc", &[json!(chest), json!(true), json!(2.0)]);
        assert!(result(&mut g));
        let item = g.inventories[&chest].items[0];
        g.time += 3.0;
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(chest), json!(bag), json!(item)]);
        assert!(!result(&mut g), "locked");
        send_scene(&mut g, 1, "InventoriesHandler.LockInventoryContentServerRpc", &[json!(chest), json!(false), json!(0.0)]);
        assert!(result(&mut g));
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(chest), json!(bag), json!(item)]);
        assert!(result(&mut g), "unlocked");
    }

    /// Picking up a scene object (a rock's Iron) or a dropped item into an inventory.
    #[test]
    fn world_objects_picked_up_go_into_the_inventory_and_out_of_the_scene() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let iron = hash(&g, "Iron");
        send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(101_000_001), json!(iron), json!(bag), json!(false)]);
        assert!(result(&mut g));
        assert!(g.inventories[&bag].items.contains(&101_000_001));
        assert!(!g.wos[&101_000_001].dont_save, "the save keeps it: the rock is gone for good");
        // An item lying in the world: its network object goes.
        let dropped = g.create_world_object(g.data.group("ice").unwrap(), 0, true);
        g.drop_on_floor(dropped, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&dropped].net.unwrap();
        let ice = hash(&g, "ice");
        send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(dropped), json!(ice), json!(bag), json!(true)]);
        assert!(result(&mut g));
        assert!(!g.net.objects.contains_key(&net) && g.wos[&dropped].net.is_none());
        // Already in an inventory (not placed): refused.
        send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(dropped), json!(ice), json!(bag), json!(true)]);
        assert!(!result(&mut g));
    }

    /// An item dropped without being taken out of its inventory first (the game's client takes
    /// it out; a hostile one need not) and then picked up into another leaves the first one.
    #[test]
    fn a_picked_up_item_leaves_the_inventory_it_was_dropped_from() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let chest = g.create_inventory(5, 0, 0, &[], &[]);
        let item = give(&mut g, "Iron", bag);
        let iron = hash(&g, "Iron");
        send_scene(&mut g, 1, "WorldObjectsHandler.DropOnFloorServerRpc", &[json!(item), json!([561.0, 3.0, 497.0]), json!(0.0), json!(false), json!(false), json!(0), json!(0.0)]);
        send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(item), json!(iron), json!(chest), json!(true)]);
        assert!(result(&mut g));
        assert!(!g.inventories[&bag].items.contains(&item) && g.inventories[&chest].items == vec![item]);
        assert_eq!(g.holders.get(&item), Some(&(chest, 1)));
    }

    /// Taking an item out and destroying it in one request still tells the inventory's listeners
    /// what left: a Vegetube's multiplier from the seeds in it goes down with each one.
    #[test]
    fn what_is_destroyed_on_leaving_an_inventory_is_still_heard_to_leave() {
        let Some((mut g, _)) = world() else { return };
        let (_, net) = place(&mut g, "Vegetube1", [570.0, 3.0, 500.0]);
        let tube = g.net.objects[&net].wo.unwrap();
        let inv = g.wos[&tube].linked_inventory;
        g.inventories.get_mut(&inv).unwrap().size = 5;
        let oxygen = crate::model::unit_index("Oxygen").unwrap();
        let (seed, seed_hash) = (g.data.group("Seed1").unwrap(), hash(&g, "Seed1"));
        let each = g.data.groups[seed].units[oxygen] / 100.0;
        assert!(each > 0.0);
        let before = g.wos[&tube].unit_multiplier[oxygen];
        let seeds: Vec<i32> = (0..4).map(|_| g.add_new_item(seed, inv, 1).1).collect();
        let multiplier = |g: &Game| g.wos[&tube].unit_multiplier[oxygen];
        assert_eq!(multiplier(&g), before + 4.0 * each);
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(seeds[0]), json!(inv), json!(true)]);
        assert!(result(&mut g) && !g.wos.contains_key(&seeds[0]));
        assert_eq!(multiplier(&g), before + 3.0 * each, "RemoveItemFromInventory");
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemsFromInventoryServerRpc", &[json!([seed_hash]), json!(inv), json!(true)]);
        assert!(result(&mut g));
        assert_eq!(multiplier(&g), before + 2.0 * each, "RemoveItemsFromInventory");
        let left = g.inventories[&inv].items[0];
        send_scene(&mut g, 1, "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc", &[json!([left]), json!(inv), json!(true)]);
        assert!(result(&mut g));
        assert_eq!(multiplier(&g), before + each, "RemoveWorldObjectsFromInventory");
        send_scene(&mut g, 1, "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", &[json!(inv)]);
        assert!(result(&mut g) && g.inventories[&inv].items.is_empty());
        assert_eq!(multiplier(&g), before, "RemoveAndDestroyAllItemsFromInventory");
    }

    /// Asking for another player's backpack or equipment as an inventory gets the empty answer
    /// the game gives for none, and does not set what it takes (a client sends the groups an
    /// inventory authorizes; only the first one to ask sets them).
    #[test]
    fn another_players_inventory_is_neither_read_nor_changed_by_getting_it() {
        let Some((mut g, _)) = world() else { return };
        join(&mut g, 2, "Bob");
        let (bag, equipment) = backpack(&g, 2);
        let iron = hash(&g, "Iron");
        let before = g.inventories[&bag].clone();
        assert!(!before.items.is_empty() && before.authorized.is_empty());
        for id in [bag, equipment] {
            let authorized = g.inventories[&id].authorized.clone();
            send_scene(&mut g, 1, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &[json!(false), json!(0), json!(id), json!(0), json!(null), json!([iron])]);
            assert_eq!(retrieved(&mut g), vec![json!(0), json!(-1), json!([]), json!([]), json!(false)]);
            assert_eq!(g.inventories[&id].authorized, authorized, "{id}");
        }
        assert!(g.inventories[&bag].authorized.is_empty());
        // Bob's own request is answered.
        send_scene(&mut g, 2, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &[json!(false), json!(0), json!(bag), json!(0), json!(null), json!(null)]);
        assert_eq!(retrieved(&mut g)[1], json!(bag));
    }

    #[test]
    fn culture_order_ignores_case_first() {
        assert_eq!(culture_cmp("ice", "Iron"), Ordering::Less);
        assert_eq!(culture_cmp("iron", "Iron"), Ordering::Less);
        assert_eq!(culture_cmp("Aluminium", "astrofood"), Ordering::Less);
    }
}
