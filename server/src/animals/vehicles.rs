//! Vehicles, server side: OwnershipSwitcher (who drives), UpdateNetworkRigidBodyState (a parked
//! vehicle goes to the nearest player, its physics on or off with distance), VehicleBackToGarage
//! (recall: the server takes the vehicle and teleports it), VehicleEquipment (modules resize
//! the vehicle's inventories) and LockNetworkPlayerPosition (seated players move with the seat).

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{json, Value};

use super::{distance2, nearest, player_distance2};
use crate::clock::GameTime;
use crate::game::{Args, Game};
use crate::model::WorldObject;
use crate::net::{enc, Target, VarValue};
use crate::wire::{Reader, Writer};

/// NetworkTransformState bits.
const POSITION: [u32; 3] = [2, 4, 8];
const ROTATION: [u32; 3] = [16, 32, 64];
const SCALE: [u32; 3] = [128, 256, 512];
const TELEPORTING: u32 = 1024;
const QUATERNION_SYNC: u32 = 4096;
const SYNCHRONIZATION: u32 = 32768;
const RELIABLE: u32 = 524288;
/// UpdateRigidBodyState's period, and its wait before the first check.
const CHECK: f32 = 0.15;
const FIRST_CHECK: f32 = 1.0;
/// VehicleBackToGarage._cooldown: at spawn, and after a recall.
const COOLDOWN_START: f32 = 5.0;
const COOLDOWN: f32 = 600.0;
/// LockNetworkPlayerPosition seats, in behaviour order: where the seat holds a player in the
/// vehicle's frame, and its turn about Y in degrees; measured on the game (the dump has no
/// transforms): the pilot's seat and the two passengers'.
const SEATS: &[(&str, &[([f32; 3], f32)])] = &[("VehicleTruck", &[([0.0, 2.065, 1.963], 0.0), ([0.696, 2.5, -1.925], 90.0), ([-0.75, 2.5, -1.925], -90.0)])];

#[derive(serde::Serialize, serde::Deserialize)]
struct Vehicle {
    /// The owner last seen, to apply the server's gain/loss of ownership whoever changed it.
    owner: u64,
    /// The server's Rigidbody.isKinematic.
    kinematic: bool,
    /// UpdateNetworkRigidBodyState._rbEnabled.
    rb_enabled: bool,
    last_position: [f32; 3],
    /// When the next rigidbody check is due (clock.rs: adding 0.15 to an f32 stops changing it at 2^22 s).
    next_check: GameTime,
    disable_distance: f32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Recall {
    vehicle: u64,
    position: [f32; 3],
    rotation: [f32; 4],
    update_rb: bool,
    kinematic: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Garage {
    cooldown: f32,
    recall: Option<Recall>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    vehicles: IndexMap<u64, Vehicle>,
    garages: IndexMap<u64, Garage>,
    /// Equipment inventory -> the vehicle's world object (VehicleEquipment).
    equipment: HashMap<i32, i32>,
    last_frame: GameTime,
    /// A seated player's object -> (where the seat put it, the owner's own last state): the
    /// game's seat overrides the copy of the player every frame, and once it lets go the copy
    /// goes back to what the owner last sent.
    seated: HashMap<u64, Seated>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Seated {
    put: [f32; 3],
    own: ([f32; 3], [f32; 4]),
}

fn component<'a>(game: &'a Game, wo: &WorldObject, ty: &str) -> Vec<&'a Value> {
    game.data.prefab_components(&game.data.groups[wo.group].id, ty)
}

fn has(game: &Game, object: u64, class: &str) -> bool {
    game.net.behaviour_index(object, class).is_some()
}

fn position(game: &Game, object: u64) -> [f32; 3] {
    game.net.objects.get(&object).map(|o| o.position).unwrap_or_default()
}

pub fn on_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let Some(object) = w.net else { return };
    if has(game, object, "OwnershipSwitcher") {
        let disable_distance = component(game, w, "UpdateNetworkRigidBodyState").first().and_then(|c| c["_disableDistance"].as_f64()).unwrap_or(30.0) as f32;
        let (owner, p) = (game.net.objects[&object].owner, w.position);
        let next_check = game.time + FIRST_CHECK;
        game.ext.animals.vehicles.vehicles.insert(object, Vehicle { owner, kinematic: false, rb_enabled: true, last_position: p, next_check, disable_distance });
        // OnNetworkSpawn in behaviour order: OwnershipSwitcher hands a server-owned vehicle to
        // the nearest client, then UpdateNetworkRigidBodyState reads the body's state.
        if owner == 0 {
            reset_ownership(game, object);
        }
        let v = game.ext.animals.vehicles.vehicles.get_mut(&object).unwrap();
        v.rb_enabled = !v.kinematic;
    }
    if has(game, object, "VehicleBackToGarage") {
        game.ext.animals.vehicles.garages.insert(object, Garage { cooldown: COOLDOWN_START, recall: None });
        game.net.set(object, "VehicleBackToGarage", "_cooldown", enc::float(COOLDOWN_START));
    }
    // VehicleEquipment watches the inventory of the InventoryAssociatedProxy beside it.
    let w = &game.wos[&wo];
    if let Some(path) = component_path(game, w, "VehicleEquipment") {
        let index = game.data.group_prefabs[&game.data.groups[w.group].id]["components"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["type"] == "InventoryAssociatedProxy" && c["path"] == path.as_str())
            .and_then(|c| c["fields"]["_secondaryInventoryIndex"].as_i64())
            .unwrap_or(-1);
        let inventory = if index < 0 { w.linked_inventory } else { w.secondary_inventories.get(index as usize).copied().unwrap_or(0) };
        game.ext.animals.vehicles.equipment.insert(inventory, wo);
    }
}

fn component_path(game: &Game, w: &WorldObject, ty: &str) -> Option<String> {
    game.data.group_prefabs[&game.data.groups[w.group].id]["components"].as_array()?.iter().find(|c| c["type"] == ty).and_then(|c| c["path"].as_str()).map(str::to_string)
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    let s = &mut game.ext.animals.vehicles;
    if let Some(object) = wo.net {
        s.vehicles.shift_remove(&object);
        s.garages.shift_remove(&object);
    }
    s.equipment.retain(|_, w| *w != wo.id);
}

pub fn rpc(game: &mut Game, client: u64, object: u64, name: &str, a: &Args) -> bool {
    match name {
        "OwnershipSwitcher.AskForOwnershipServerRpc" => {
            let locked = bool_var(game, object, "OwnershipSwitcher", "_locked");
            if !locked {
                if game.net.objects.get(&object).is_some_and(|o| o.owner != client) {
                    set_owner(game, object, client);
                }
                game.net.set(object, "OwnershipSwitcher", "_locked", enc::bool(true));
            }
            game.net.rpc(Target::Only(client), object, "OwnershipSwitcher.AskForOwnershipResultClientRpc", &[json!(!locked)]);
        }
        "OwnershipSwitcher.ResetOwnershipServerRpc" => reset_ownership(game, object),
        "UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc" => {
            game.net.rpc(Target::All, object, "UpdateNetworkRigidBodyState.SetVehicleControllerEnabledClientRpc", &[json!(a.b(0))]);
        }
        "VehicleBackToGarage.MoveVehicleToPositionServerRpc" => move_vehicle_to_position(game, object, a.u(0), a.v3(1), a.q(2)),
        _ => return false,
    }
    true
}

fn bool_var(game: &Game, object: u64, class: &str, var: &str) -> bool {
    matches!(game.net.get(object, class, var), Some(VarValue::Plain(b)) if b.first() == Some(&1))
}

/// ResetOwnershipServerRpc: the nearest client drives, nobody holds the lock.
fn reset_ownership(game: &mut Game, object: u64) {
    let nearest = game.nearest_client(position(game, object));
    if game.net.objects.get(&object).is_some_and(|o| o.owner != nearest) {
        set_owner(game, object, nearest);
    }
    game.net.set(object, "OwnershipSwitcher", "_locked", enc::bool(false));
}

/// NetworkObject.ChangeOwnership as the server, with what the server's behaviours do when it
/// gains or loses an object.
pub fn set_owner(game: &mut Game, object: u64, owner: u64) {
    game.net.change_owner(object, owner);
    owner_changed(game, object, owner);
}

fn owner_changed(game: &mut Game, object: u64, owner: u64) {
    let Some(v) = game.ext.animals.vehicles.vehicles.get_mut(&object) else { return };
    let previous = std::mem::replace(&mut v.owner, owner);
    if previous == owner {
        return;
    }
    if previous == 0 {
        // UpdateNetworkRigidBodyState.OnOwnershipChanged: the body stops on the server.
        v.kinematic = true;
    }
    if owner == 0 {
        // OnGainedOwnership: UpdateNetworkRigidBodyState rereads the body, OwnershipSwitcher unlocks.
        v.rb_enabled = !v.kinematic;
        game.net.set(object, "OwnershipSwitcher", "_locked", enc::bool(false));
    }
}

/// Ownership changed elsewhere (a leaving client's objects go back to the server).
pub fn watch_owners(game: &mut Game) {
    let changed: Vec<(u64, u64)> = game.ext.animals.vehicles.vehicles.iter().filter_map(|(o, v)| game.net.objects.get(o).filter(|n| n.owner != v.owner).map(|n| (*o, n.owner))).collect();
    for (object, owner) in changed {
        owner_changed(game, object, owner);
    }
}

/// NetworkManager.OnClientDisconnectCallback: seats the client held are freed; the objects it
/// owned are the server's (GroupNetworkContainer.OnGainedOwnership turns transform sync off).
pub fn client_left(game: &mut Game, client: u64) {
    let seats: Vec<(u64, u16)> = game
        .net
        .objects
        .values()
        .flat_map(|o| o.behaviours.iter().filter(|b| b.class == "LockNetworkPlayerPosition").map(move |b| (o.id, b.index)))
        .filter(|(o, b)| seat(game, *o, *b) == Some(client))
        .collect();
    for (object, index) in seats {
        game.net.set_behaviour_var(object, index, "_active", enc::bool(false));
        game.net.set_behaviour_var(object, index, "_playerId", enc::ulong(0));
    }
    let containers: Vec<u64> = game
        .net
        .objects
        .values()
        .filter(|o| o.owner == 0 && bool_var(game, o.id, "GroupNetworkContainer", "_transformSync"))
        .filter(|o| !o.wo.and_then(|w| game.wos.get(&w)).is_some_and(|w| component(game, w, "GroupNetworkContainer").first().is_some_and(|c| c["_alwaysKeepRigidBodyAndNetworkTransformActive"] == true)))
        .map(|o| o.id)
        .collect();
    for object in containers {
        game.net.set(object, "GroupNetworkContainer", "_transformSync", enc::bool(false));
    }
}

/// The client locked into a seat (LockNetworkPlayerPosition behaviour), if active.
fn seat(game: &Game, object: u64, index: u16) -> Option<u64> {
    let active = matches!(game.net.behaviour_var(object, index, "_active"), Some(VarValue::Plain(b)) if b.first() == Some(&1));
    match game.net.behaviour_var(object, index, "_playerId") {
        Some(VarValue::Plain(b)) if active => Reader::new(b).packed_u64(),
        _ => None,
    }
}

/// MoveVehicleToPositionServerRpc: an unlocked vehicle is recalled to the garage.
fn move_vehicle_to_position(game: &mut Game, garage: u64, vehicle: u64, position: [f32; 3], rotation: [f32; 4]) {
    if !game.net.objects.contains_key(&vehicle) || !has(game, vehicle, "OwnershipSwitcher") || bool_var(game, vehicle, "OwnershipSwitcher", "_locked") {
        return;
    }
    let garage_owner = game.net.objects.get(&garage).map(|o| o.owner).unwrap_or(0);
    if game.net.objects[&vehicle].owner != 0 {
        set_owner(game, vehicle, garage_owner);
    }
    let Some(g) = game.ext.animals.vehicles.garages.get_mut(&garage) else { return };
    g.recall = Some(Recall { vehicle, position, rotation, update_rb: false, kinematic: false });
    g.cooldown = COOLDOWN;
    game.net.set(garage, "VehicleBackToGarage", "_cooldown", enc::float(COOLDOWN));
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    let Some(vehicle) = game.ext.animals.vehicles.equipment.get(&inventory).copied() else { return };
    // An item destroyed out of the equipment is still there: removals propagate before they destroy.
    let (Some(item), Some(v)) = (game.wos.get(&wo), game.wos.get(&vehicle)) else { return };
    let g = &game.data.groups[item.group];
    let value = g.fields["value"].as_i64().unwrap_or(0) as i32;
    let delta = if added { value } else { -value };
    let at = v.net.map(|o| position(game, o)).unwrap_or(v.position);
    let linked = v.linked_inventory;
    let resize = |game: &mut Game, target: i32| {
        if game.inventories.get(&target).is_some_and(|i| i.size + delta > 1) {
            game.inventories_rpc(0, "InventoriesHandler.SetInventorySizeServerRpc", &Args(vec![json!(target), json!(delta), json!(at)]));
        }
    };
    let ty = g.str("equipableType").to_string();
    match ty.as_str() {
        "BackpackIncrease" => resize(game, linked),
        "EquipmentIncrease" => resize(game, inventory),
        "LogisticChip" if !added => {
            let priority = game.inventories.get(&linked).map(|i| i.logistic.priority).unwrap_or(0);
            game.inventories_rpc(0, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &Args(vec![json!(linked), json!([]), json!([]), json!(priority)]));
        }
        _ => {}
    }
}

/// One frame: garages (LateUpdate), parked vehicles (UpdateRigidBodyState), seats (LateUpdate).
pub fn frame(game: &mut Game) {
    let dt = game.time - std::mem::replace(&mut game.ext.animals.vehicles.last_frame, game.time);
    // Objects despawned without their world object being destroyed.
    let s = &mut game.ext.animals.vehicles;
    s.vehicles.retain(|o, _| game.net.objects.contains_key(o));
    s.garages.retain(|o, _| game.net.objects.contains_key(o));
    let garages: Vec<u64> = game.ext.animals.vehicles.garages.keys().copied().collect();
    for garage in garages {
        garage_frame(game, garage, dt);
    }
    let vehicles: Vec<u64> = game.ext.animals.vehicles.vehicles.keys().copied().collect();
    for object in vehicles {
        let v = &game.ext.animals.vehicles.vehicles[&object];
        if game.time >= v.next_check {
            let v = game.ext.animals.vehicles.vehicles.get_mut(&object).unwrap();
            v.next_check.advance(f64::from(CHECK));
            rigidbody_check(game, object);
        }
        // LockNetworkPlayerPosition.LateUpdate: a seated player is where the seat is.
        let Some(o) = game.net.objects.get(&object) else { continue };
        let (p, turn) = (o.position, o.rotation);
        let group = o.wo.and_then(|w| game.wos.get(&w)).map(|w| game.data.groups[w.group].id.clone()).unwrap_or_default();
        let seats = SEATS.iter().find(|(g, _)| *g == group).map(|(_, s)| *s).unwrap_or(&[]);
        let indices: Vec<u16> = o.behaviours.iter().filter(|b| b.class == "LockNetworkPlayerPosition").map(|b| b.index).collect();
        for (k, index) in indices.into_iter().enumerate() {
            let Some(player) = seat(game, object, index).and_then(|c| game.player_object(c)) else { continue };
            let (offset, yaw) = seats.get(k).copied().unwrap_or(([0.0; 3], 0.0));
            let at = { let r = crate::geometry::rotate(turn, offset); [p[0] + r[0], p[1] + r[1], p[2] + r[2]] };
            let facing = crate::geometry::mul(turn, crate::geometry::from_euler_y(yaw));
            let Some(now) = game.net.objects.get(&player) else { continue };
            // The owner's state is whatever is there but what this seat put there last.
            let own = match game.ext.animals.vehicles.seated.get(&player) {
                Some(s) if s.put == now.position => None,
                _ => Some((now.position, now.rotation)),
            };
            let entry = game.ext.animals.vehicles.seated.entry(player).or_insert(Seated { put: at, own: (now.position, now.rotation) });
            entry.put = at;
            if let Some(own) = own {
                entry.own = own;
            }
            game.net.move_object(player, at, Some(facing));
        }
    }
    // A seat that let go: the copy of the player returns to the owner's last state.
    let free: Vec<u64> = {
        let held: Vec<u64> = game.ext.animals.vehicles.vehicles.keys().flat_map(|v| seated_players(game, *v)).collect();
        game.ext.animals.vehicles.seated.keys().filter(|p| !held.contains(p)).copied().collect()
    };
    for player in free {
        if let Some(s) = game.ext.animals.vehicles.seated.remove(&player) {
            game.net.move_object(player, s.own.0, Some(s.own.1));
        }
    }
}

/// The player objects locked into the seats of a vehicle.
fn seated_players(game: &Game, object: u64) -> Vec<u64> {
    let Some(o) = game.net.objects.get(&object) else { return Vec::new() };
    o.behaviours.iter().filter(|b| b.class == "LockNetworkPlayerPosition").filter_map(|b| seat(game, object, b.index)).filter_map(|c| game.player_object(c)).collect()
}

fn garage_frame(game: &mut Game, garage: u64, dt: f32) {
    let g = game.ext.animals.vehicles.garages.get_mut(&garage).unwrap();
    if g.cooldown > 0.0 {
        g.cooldown = (g.cooldown - dt).max(0.0);
        let value = g.cooldown;
        game.net.set(garage, "VehicleBackToGarage", "_cooldown", enc::float(value));
    }
    let g = game.ext.animals.vehicles.garages.get_mut(&garage).unwrap();
    let Some(r) = g.recall.as_mut() else { return };
    let vehicle = r.vehicle;
    let Some(owner) = game.net.objects.get(&vehicle).map(|o| o.owner) else {
        g.recall = None;
        return;
    };
    if owner != 0 {
        let garage_owner = game.net.objects.get(&garage).map(|o| o.owner).unwrap_or(0);
        set_owner(game, vehicle, garage_owner);
        return;
    }
    let Some(v) = game.ext.animals.vehicles.vehicles.get_mut(&vehicle) else { return };
    if !r.update_rb {
        r.kinematic = v.kinematic;
        v.kinematic = true;
        r.update_rb = true;
    }
    let (target, rotation) = (r.position, r.rotation);
    if distance2(position(game, vehicle), target).sqrt() > 1.0 {
        teleport(game, vehicle, target, rotation);
        return;
    }
    let g = game.ext.animals.vehicles.garages.get_mut(&garage).unwrap();
    let kinematic = g.recall.take().map(|r| r.kinematic).unwrap_or(false);
    if let Some(v) = game.ext.animals.vehicles.vehicles.get_mut(&vehicle) {
        v.kinematic = kinematic;
    }
}

/// UpdateRigidBodyState, one pass: a vehicle at rest goes to the nearest player when its owner
/// is far; its owner turns physics on only with a player near.
fn rigidbody_check(game: &mut Game, object: u64) {
    let p = position(game, object);
    let v = &game.ext.animals.vehicles.vehicles[&object];
    if distance2(v.last_position, p).sqrt() < 3.0 {
        let disable = v.disable_distance;
        let (closest, d2) = nearest(game, p);
        let num = d2.sqrt();
        let owner = game.net.objects.get(&object).map(|o| o.owner).unwrap_or(0);
        let num2 = player_distance2(game, owner, p).sqrt();
        if num2 > disable && num < num2 {
            set_owner(game, object, closest);
        }
        let enabled = num < disable;
        let v = game.ext.animals.vehicles.vehicles.get_mut(&object).unwrap();
        if enabled != v.rb_enabled {
            v.rb_enabled = enabled;
            let owner = game.net.objects.get(&object).map(|o| o.owner).unwrap_or(0);
            if owner == 0 {
                v.kinematic = !enabled;
            }
            game.net.rpc(Target::Only(owner), object, "UpdateNetworkRigidBodyState.SetRigidBodyEnabledClientRpc", &[json!(enabled)]);
        }
    }
    game.ext.animals.vehicles.vehicles.get_mut(&object).unwrap().last_position = p;
}

/// NetworkTransform.Teleport by the server as the vehicle's owner: one teleporting state to
/// every client, laid out like the object's own transform state.
fn teleport(game: &mut Game, object: u64, position: [f32; 3], rotation: [f32; 4]) {
    let Some(o) = game.net.objects.get(&object) else { return };
    let Some(behaviour) = o.behaviours.iter().find(|b| b.class.ends_with("NetworkTransform")).map(|b| b.index) else { return };
    let bits = (transform_bits(&o.tail, behaviour).unwrap_or(POSITION.iter().chain(&ROTATION).sum()) & !SYNCHRONIZATION) | TELEPORTING | RELIABLE;
    let message = transform_message(object, behaviour, bits, game.tick_number(), position, rotation, o.scale);
    game.out_to(Target::All, (crate::protocol::NETWORK_TRANSFORM, message));
    game.net.move_object(object, position, Some(rotation));
    if let Some(wo) = game.net.objects.get(&object).and_then(|o| o.wo).and_then(|w| game.wos.get_mut(&w)) {
        wo.position = position;
        wo.rotation = rotation;
    }
}

/// NetworkTransformMessage: object, behaviour, then the state with the axes `bits` lists.
fn transform_message(object: u64, behaviour: u16, bits: u32, tick: i32, position: [f32; 3], rotation: [f32; 4], scale: [f32; 3]) -> Vec<u8> {
    let mut w = Writer::new();
    w.packed_u64(object).packed_i32(behaviour as i32).packed_u32(bits).packed_i32(tick);
    for (axis, bit) in POSITION.iter().enumerate() {
        if bits & bit != 0 {
            w.f32(position[axis]);
        }
    }
    if bits & QUATERNION_SYNC != 0 {
        w.quat(rotation);
    } else {
        let euler = euler_angles(rotation);
        for (axis, bit) in ROTATION.iter().enumerate() {
            if bits & bit != 0 {
                w.f32(euler[axis]);
            }
        }
    }
    for (axis, bit) in SCALE.iter().enumerate() {
        if bits & bit != 0 {
            w.f32(scale[axis]);
        }
    }
    w.0
}

/// The state bits of a behaviour's transform state in an object's custom sync data.
fn transform_bits(tail: &[u8], behaviour: u16) -> Option<u32> {
    let mut r = Reader::new(tail);
    let count = r.u8()?;
    for _ in 0..count {
        let index = r.u16_raw()?;
        let len = r.i32()? as usize;
        let start = r.pos;
        if index == behaviour {
            return r.packed_u32();
        }
        r.pos = start + len;
    }
    None
}

/// Quaternion.eulerAngles: degrees in [0, 360), Z then X then Y.
fn euler_angles(q: [f32; 4]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let sin_x = (2.0 * (w * x - y * z)).clamp(-1.0, 1.0);
    let ex = sin_x.asin();
    let (ey, ez) = if sin_x.abs() < 0.9999 {
        ((2.0 * (w * y + x * z)).atan2(1.0 - 2.0 * (x * x + y * y)), (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (x * x + z * z)))
    } else {
        // Gimbal lock: all yaw, no roll.
        (2.0 * y.atan2(w), 0.0)
    };
    [ex, ey, ez].map(|a| a.to_degrees().rem_euclid(360.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{client_rpcs, send_rpc};

    #[test]
    fn euler_angles_like_unity() {
        let e = euler_angles([0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2]);
        assert!((e[0]).abs() < 1e-3 && (e[1] - 90.0).abs() < 1e-3 && e[2].abs() < 1e-3, "{e:?}");
        let e = euler_angles(crate::geometry::from_euler_y(-30.0));
        assert!((e[1] - 330.0).abs() < 1e-3, "{e:?}");
        let h = std::f32::consts::FRAC_PI_8;
        let e = euler_angles([h.sin(), 0.0, 0.0, h.cos()]);
        assert!((e[0] - 45.0).abs() < 1e-3, "{e:?}");
    }

    #[test]
    fn teleports_read_back_like_client_transforms() {
        let bits = (126 | 2048 | 1048576) | TELEPORTING | RELIABLE;
        let m = transform_message(52, 0, bits, 41963, [553.0, 4.0, 491.0], crate::geometry::from_euler_y(90.0), [1.0; 3]);
        let (object, p, r) = crate::transform::position_rotation(&m).unwrap();
        assert_eq!((object, p), (52, [553.0, 4.0, 491.0]));
        assert!((crate::geometry::euler_y(r.unwrap()) - 90.0).abs() < 1e-3);
        assert_eq!(m.len(), 2 + 1 + 3 + 3 + 24);
    }

    /// Driving the rover as ActionTakeControl does. Its ownership callback first reads the
    /// equipment VehicleEquipment got from the InventoryEquipment proxy at spawn: without that
    /// answer the inventory is null, the callback throws, the player never sits down and the
    /// rover stays locked. Then the controller goes on, the pilot seat holds the player, the seat
    /// carries the player along, and getting out unlocks the rover.
    #[test]
    fn the_rover_can_be_entered_driven_and_left() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let player = crate::game::tests::join(&mut g, 1, "Ann");
        let truck = g.data.group("VehicleTruck").unwrap();
        let (id, net) = g.create_and_instantiate(truck, [560.0, 3.0, 494.0], [0.0, 0.0, 0.0, 1.0], false, true, false);
        let components = g.data.group_prefabs["VehicleTruck"]["components"].as_array().unwrap();
        let nth = components.iter().filter(|c| c["type"] == "InventoryAssociatedProxy").position(|c| c["path"] == "InventoryEquipment").unwrap();
        let proxy = g.net.objects[&net].behaviours.iter().filter(|b| b.class == "InventoryAssociatedProxy").nth(nth).unwrap().index;
        g.net.out.clear();
        send_rpc(&mut g, 1, net, proxy, "InventoryAssociatedProxy.GetInventoryIdServerRpc", &[]);
        let answers: Vec<(u16, Value)> = client_rpcs(&g, "InventoryAssociatedProxy.GetInventoryIdClientRpc").into_iter().map(|(b, a)| (b, a[2].clone())).collect();
        assert_eq!(answers, vec![(proxy, json!(g.wos[&id].secondary_inventories[0]))], "VehicleEquipment's proxy gets the equipment");

        let index = |class| g.net.behaviour_index(net, class).unwrap();
        let (switcher, body, pilot) = (index("OwnershipSwitcher"), index("UpdateNetworkRigidBodyState"), index("LockNetworkPlayerPosition"));
        g.net.out.clear();
        send_rpc(&mut g, 1, net, switcher, "OwnershipSwitcher.AskForOwnershipServerRpc", &[]);
        assert_eq!(client_rpcs(&g, "OwnershipSwitcher.AskForOwnershipResultClientRpc"), vec![(switcher, vec![json!(true)])]);
        send_rpc(&mut g, 1, net, body, "UpdateNetworkRigidBodyState.SetVehicleControllerEnabledServerRpc", &[json!(true)]);
        assert_eq!(client_rpcs(&g, "UpdateNetworkRigidBodyState.SetVehicleControllerEnabledClientRpc"), vec![(body, vec![json!(true)])]);
        send_rpc(&mut g, 1, net, pilot, "LockNetworkPlayerPosition.SetPlayerLockedServerRpc", &[json!(true), json!(1)]);
        assert_eq!((g.net.objects[&net].owner, seat(&g, net, pilot)), (1, Some(1)));

        // The driver's transforms move the rover; the seat takes the player along.
        let mut w = Writer::new();
        w.packed_u64(net).packed_i32(0).packed_u32(2 | 4 | 8).packed_i32(1).f32(600.0).f32(3.0).f32(494.0);
        g.receive(1, &crate::wire::batch(&[(crate::protocol::NETWORK_TRANSFORM, w.0)]));
        g.time += 1.0;
        crate::animals::tick(&mut g);
        // The pilot's seat holds the player 2.065 m up and 1.963 m ahead of the rover's origin.
        assert_eq!((g.net.objects[&player].position, g.net.objects[&net].owner), ([600.0, 5.065, 495.963], 1));

        // The seat only overrides the copy of the player: the owner's own states are kept, and
        // once the seat lets go the copy is where the owner last put it.
        let mut w = Writer::new();
        w.packed_u64(player).packed_i32(0).packed_u32(2 | 4 | 8).packed_i32(1).f32(610.0).f32(4.0).f32(500.0);
        g.receive(1, &crate::wire::batch(&[(crate::protocol::NETWORK_TRANSFORM, w.0)]));
        g.time += 1.0;
        crate::animals::tick(&mut g);
        assert_eq!(g.net.objects[&player].position, [600.0, 5.065, 495.963], "the seat wins while it holds");

        // GetOut: the seat is freed and the rover unlocked for the next driver.
        send_rpc(&mut g, 1, net, pilot, "LockNetworkPlayerPosition.SetPlayerLockedServerRpc", &[json!(false), json!(0)]);
        send_rpc(&mut g, 1, net, switcher, "OwnershipSwitcher.ResetOwnershipServerRpc", &[]);
        assert_eq!((seat(&g, net, pilot), bool_var(&g, net, "OwnershipSwitcher", "_locked")), (None, false));
        g.time += 1.0;
        crate::animals::tick(&mut g);
        assert_eq!(g.net.objects[&player].position, [610.0, 4.0, 500.0], "back where the owner last put the player");
    }

    /// The passengers' seats of the rover: beside the pilot's and turned a quarter either way.
    #[test]
    fn the_rovers_passenger_seats_hold_a_player_turned_aside() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let player = crate::game::tests::join(&mut g, 1, "Ann");
        let truck = g.data.group("VehicleTruck").unwrap();
        let (_, net) = g.create_and_instantiate(truck, [560.0, 3.0, 494.0], [0.0, 0.0, 0.0, 1.0], false, true, false);
        let seats: Vec<u16> = g.net.objects[&net].behaviours.iter().filter(|b| b.class == "LockNetworkPlayerPosition").map(|b| b.index).collect();
        assert_eq!(seats.len(), 3);
        for (seat_index, (expect, yaw)) in seats.iter().skip(1).zip([([560.696, 5.5, 492.075], 90.0), ([559.25, 5.5, 492.075], -90.0)]) {
            send_rpc(&mut g, 1, net, *seat_index, "LockNetworkPlayerPosition.SetPlayerLockedServerRpc", &[json!(true), json!(1)]);
            g.time += 1.0;
            crate::animals::tick(&mut g);
            let o = &g.net.objects[&player];
            assert!((0..3).all(|i| (o.position[i] - expect[i]).abs() < 1e-3), "{:?}", o.position);
            let want = crate::geometry::from_euler_y(yaw);
            assert!((0..4).all(|i| (o.rotation[i] - want[i]).abs() < 1e-3), "{:?} {want:?}", o.rotation);
            send_rpc(&mut g, 1, net, *seat_index, "LockNetworkPlayerPosition.SetPlayerLockedServerRpc", &[json!(false), json!(0)]);
        }
    }

    /// Recalling a vehicle to its garage: refused while someone drives it; else the server takes
    /// it, teleports it to the spot, and the garage cools down for ten minutes.
    #[test]
    fn vehicles_recalled_to_their_garage_are_taken_and_teleported() {
        use crate::testkit::*;
        let Some((mut g, _)) = world() else { return };
        let (_, garage) = place(&mut g, "VehicleCrafter1", [570.0, 3.0, 500.0]);
        let (_, truck) = place(&mut g, "VehicleTruck", [600.0, 3.0, 494.0]);
        assert_eq!(g.net.objects[&truck].owner, 1, "parked: the nearest player simulates it");
        let target = [572.0, 3.0, 506.0];
        let rotation = crate::geometry::from_euler_y(90.0);
        send_on(&mut g, 1, truck, "OwnershipSwitcher.AskForOwnershipServerRpc", &[]);
        send_on(&mut g, 1, garage, "VehicleBackToGarage.MoveVehicleToPositionServerRpc", &[json!(truck), json!(target), json!(rotation)]);
        assert_eq!(var(&g, garage, "VehicleBackToGarage", "_cooldown"), enc::float(COOLDOWN_START), "driven: no recall");
        send_on(&mut g, 1, truck, "OwnershipSwitcher.ResetOwnershipServerRpc", &[]);
        send_on(&mut g, 1, garage, "VehicleBackToGarage.MoveVehicleToPositionServerRpc", &[json!(truck), json!(target), json!(rotation)]);
        assert_eq!(var(&g, garage, "VehicleBackToGarage", "_cooldown"), enc::float(COOLDOWN));
        assert_eq!(g.net.objects[&truck].owner, 0, "the server takes it");
        for _ in 0..3 {
            g.time += 0.05;
            crate::animals::tick(&mut g);
        }
        assert_eq!(g.net.objects[&truck].position, target);
        assert!(g.ext.animals.vehicles.garages[&garage].recall.is_none(), "done");
        let cooldown = f32::from_le_bytes(var(&g, garage, "VehicleBackToGarage", "_cooldown")[..4].try_into().unwrap());
        assert!(cooldown < COOLDOWN && cooldown > COOLDOWN - 1.0, "{cooldown}");
        // Not a vehicle: nothing.
        send_on(&mut g, 1, garage, "VehicleBackToGarage.MoveVehicleToPositionServerRpc", &[json!(garage), json!(target), json!(rotation)]);
        assert!(g.ext.animals.vehicles.garages[&garage].recall.is_none());
    }

    /// Modules in a vehicle's equipment resize it as the game's VehicleEquipment does: storage
    /// modules the storage, equipment modules the equipment; taking out the logistics chip
    /// clears the vehicle's logistics. Shrunk below what it holds, the rest drops beside it.
    #[test]
    fn vehicle_modules_resize_its_inventories() {
        use crate::testkit::*;
        let Some((mut g, _)) = world() else { return };
        let (truck, _) = place(&mut g, "VehicleTruck", [600.0, 3.0, 494.0]);
        let (storage, equipment) = (g.wos[&truck].linked_inventory, g.wos[&truck].secondary_inventories[0]);
        let size = |g: &Game, i: i32| g.inventories[&i].size;
        let (storage_size, equipment_size) = (size(&g, storage), size(&g, equipment));
        let crate_module = give(&mut g, "VehicleInventorySize1", equipment);
        g.propagate(equipment, crate_module, true);
        assert_eq!(size(&g, storage), storage_size + 18);
        let slot = give(&mut g, "VehicleEquipmentSize1", equipment);
        g.propagate(equipment, slot, true);
        assert_eq!(size(&g, equipment), equipment_size + 3);
        // The storage filled past its old size, then the module out: the rest drops.
        let fill: Vec<i32> = (0..storage_size + 2).map(|_| give(&mut g, "Iron", storage)).collect();
        g.inventory_remove(equipment, crate_module);
        g.propagate(equipment, crate_module, false);
        assert_eq!(size(&g, storage), storage_size);
        assert_eq!(g.inventories[&storage].items.len() as i32, storage_size);
        assert!(fill.iter().rev().take(2).all(|i| g.wos[i].is_placed()), "the two over dropped");
        // The logistics chip out: the storage's logistics are cleared.
        let iron = g.data.group("Iron").unwrap();
        g.inventories.get_mut(&storage).unwrap().logistic.supply = vec![iron];
        let chip = give(&mut g, "VehicleLogistic1", equipment);
        g.inventory_remove(equipment, chip);
        g.propagate(equipment, chip, false);
        assert!(g.inventories[&storage].logistic.supply.is_empty());
    }

    /// A module taken out of the equipment and destroyed in one request (the trash) still gives
    /// back what it gave: the storage shrinks again.
    #[test]
    fn a_vehicle_module_destroyed_out_of_its_equipment_resizes_the_vehicle_back() {
        use crate::testkit::*;
        let Some((mut g, _)) = world() else { return };
        let (truck, _) = place(&mut g, "VehicleTruck", [600.0, 3.0, 494.0]);
        let (storage, equipment) = (g.wos[&truck].linked_inventory, g.wos[&truck].secondary_inventories[0]);
        let storage_size = g.inventories[&storage].size;
        let module = give(&mut g, "VehicleInventorySize1", equipment);
        g.propagate(equipment, module, true);
        assert_eq!(g.inventories[&storage].size, storage_size + 18);
        send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(module), json!(equipment), json!(true)]);
        assert!(!g.wos.contains_key(&module));
        assert_eq!(g.inventories[&storage].size, storage_size);
    }

    /// A parked vehicle goes to a player near it when its owner is far; physics are on only
    /// with a player near, and the owner hears each change.
    #[test]
    fn parked_vehicles_go_to_a_near_player_and_sleep_with_nobody_near() {
        use crate::testkit::*;
        let Some((mut g, ann)) = world() else { return };
        let bob = join(&mut g, 2, "Bob");
        let (_, truck) = place(&mut g, "VehicleTruck", [600.0, 3.0, 494.0]);
        assert_eq!(g.net.objects[&truck].owner, 1);
        let frames = |g: &mut Game, seconds: f32| {
            let end = g.time + seconds;
            while g.time < end {
                g.time += 0.05;
                crate::animals::tick(g);
            }
        };
        // Ann goes far away, Bob stands by the truck.
        g.net.move_object(ann, [5000.0, 3.0, 5000.0], None);
        g.net.move_object(bob, [605.0, 3.0, 494.0], None);
        frames(&mut g, 2.0);
        assert_eq!(g.net.objects[&truck].owner, 2, "the near player simulates it");
        // Nobody near: its physics go off.
        g.net.out.clear();
        g.net.move_object(bob, [5000.0, 3.0, 4000.0], None);
        frames(&mut g, 2.0);
        let off = sent(&g, "UpdateNetworkRigidBodyState.SetRigidBodyEnabledClientRpc");
        assert_eq!(off.into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(2), vec![json!(false)])]);
        g.net.out.clear();
        g.net.move_object(bob, [601.0, 3.0, 494.0], None);
        frames(&mut g, 2.0);
        assert_eq!(sent(&g, "UpdateNetworkRigidBodyState.SetRigidBodyEnabledClientRpc").last().map(|s| s.args.clone()), Some(vec![json!(true)]));
    }

    #[test]
    fn reads_the_trucks_transform_bits() {
        // VehicleTruck's dumped sync tail: ClientNetworkTransform (behaviour 0) state.
        let tail = crate::world::hex("0100001d000000f363c482040000000000409cc500000000000000800000000000000000");
        let bits = transform_bits(&tail, 0).unwrap();
        assert_eq!(bits & 126, 126, "position and euler rotation");
        assert_eq!(bits & (QUATERNION_SYNC | 16384), 0, "full floats, euler angles");
        assert!(transform_bits(&tail, 1).is_none());
    }
}
