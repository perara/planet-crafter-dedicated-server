use super::*;
use crate::save::SaveFile;

fn game() -> Option<Game> {
    let mut save = SaveFile::empty();
    save.settings = json!({"planetId": "Prime"});
    save.world = json!({"logisticsPaused": false});
    save.planets = vec![json!({"planetId": "Prime"})];
    crate::game::test_game(save)
}

/// What CreateAndInstantiateWorldObjectServerRpc does for a building.
fn build(game: &mut Game, group: &str, position: V3) -> i32 {
    let g = game.data.group(group).unwrap();
    let id = game.create_world_object(g, 0, true);
    let planet = game.planet_hash();
    let w = game.wos.get_mut(&id).unwrap();
    w.position = position;
    w.rotation = motion::IDENTITY;
    w.planet = planet;
    game.instantiate(id, false, 0).unwrap();
    id
}

fn add(game: &mut Game, group: &str, inventory: i32) -> i32 {
    let g = game.data.group(group).unwrap();
    let (ok, wo) = game.add_new_item(g, inventory, 0);
    assert!(ok);
    wo
}

fn group_items(game: &Game, inventory: i32, group: &str) -> usize {
    let g = game.data.group(group).unwrap();
    items(game, inventory).iter().filter(|w| game.wos[*w].group == g).count()
}

/// The oracle scenario: a station with one Drone1, a supply container with two iron and a
/// demand container asking for iron. The drone carries both, one at a time, and goes home.
#[test]
fn a_drone_carries_supply_to_demand_and_goes_home() {
    let Some(mut game) = game() else { return };
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let (station_inv, supply_inv, demand_inv) = (game.wos[&station].linked_inventory, game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    add(&mut game, "Iron", supply_inv);
    add(&mut game, "Iron", supply_inv);
    game.inventories.get_mut(&supply_inv).unwrap().logistic.supply = vec![iron];
    set_dirty(&mut game, supply_inv);
    game.inventories.get_mut(&demand_inv).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, demand_inv);
    let drone = add(&mut game, "Drone1", station_inv);
    assert!(crate::energy::has_enough(&game, game.planet_hash()));

    game.net.out.clear();
    let start = game.ext.logistics.last_tick;
    let mut transforms = 0;
    let mut spawned_at = None;
    let mut most_tasks = 0;
    for t in 1..=40 * 30 {
        frame(&mut game, start + t);
        for (_, kind, body) in std::mem::take(&mut game.net.out) {
            if kind == NETWORK_TRANSFORM {
                transforms += 1;
            }
            if kind == CREATE_OBJECT && spawned_at.is_none() {
                spawned_at = Some(t);
                // The spawn carries the NetworkTransform synchronization (flags 0x88c2e).
                assert!(body.windows(3).any(|w| w == [0x73, 0x61, 0x44]), "{body:02x?}");
            }
        }
        most_tasks = most_tasks.max(game.ext.logistics.all_tasks.iter().count());
    }
    assert!(game.ext.logistics.enabled);
    assert_eq!(most_tasks, 2);
    // Released by the first SetLogisticTasks after one second, in its second frame (no drone
    // flew yet, so there is no access array and no frame for assigning flying drones).
    assert_eq!(spawned_at, Some(30 + 1));
    assert!(transforms > 50, "{transforms}");
    assert_eq!(group_items(&game, demand_inv, "Iron"), 2);
    assert_eq!(group_items(&game, supply_inv, "Iron"), 0);
    assert!(items(&game, station_inv).contains(&drone), "the drone is back in its station");
    assert!(game.ext.logistics.fleet.is_empty());
    assert_eq!(game.ext.logistics.entities[&demand_inv].waiting, 0);
    assert!(game.wos[&drone].net.is_none() && game.ext.logistics.drones[&drone].parked.is_some());
    // Container1 counts for interplanetary depots (rockets::interplanetary): still two iron on
    // Prime, now in the demand one; nothing counted twice nor what the drone carried.
    let class = "LogisticInterplanetaryManager";
    let (object, _) = game.net.find_class(class).unwrap();
    let Some(crate::net::VarValue::List(list)) = game.net.get(object, class, "_itemsInPlanetaryLogisticsGroups") else { panic!() };
    let mut expected = game.data.groups[iron].hash.to_le_bytes().to_vec();
    expected.extend(game.planet_hash().to_le_bytes());
    expected.extend(2i32.to_le_bytes());
    assert_eq!(list, &vec![expected]);

    // The parked drone flies again for the next demand, from where it docked.
    add(&mut game, "Iron", supply_inv);
    for t in 1..=60 {
        frame(&mut game, start + 40 * 30 + t);
    }
    assert!(game.wos[&drone].net.is_some(), "spawned again");
    assert!(game.ext.logistics.fleet.contains(&drone));
}

#[test]
fn slots_reuse_the_last_freed_entry_like_dotnet() {
    let mut s: Slots<i32, ()> = Slots::default();
    for k in [1, 2, 3, 4] {
        s.insert(k, ());
    }
    s.remove(&2);
    s.remove(&4);
    s.insert(5, ());
    s.insert(6, ());
    s.insert(7, ());
    assert_eq!(s.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![1, 6, 3, 5, 7]);
}

/// More than 16 demands of one priority: List<T>.Sort is an introsort, so they end in the order
/// reference.py gives for 20 equal keys (equal20 of tests/dotnet_sort/vectors.txt), not the order
/// they were put in. (On the real game, 17 demands at one priority and 8 items to give them: the
/// same 8 of the 17 are served each time, and the same 8 here: completeness/demand_order.py.)
#[test]
fn equal_priority_demands_are_sorted_like_the_games_introsort() {
    let Some(mut game) = game() else { return };
    build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let iron = game.data.group("Iron").unwrap();
    for i in 0..20 {
        let w = build(&mut game, "Container1", [25.0, 2.0, 30.0 + 4.0 * i as f32]);
        let inv = game.wos[&w].linked_inventory;
        game.inventories.get_mut(&inv).unwrap().logistic.demand = vec![iron];
        set_dirty(&mut game, inv);
    }
    // AddNewDemandInventory puts an equal priority before the others: the last one is first.
    let put_in = game.ext.logistics.demand.clone();
    assert_eq!(put_in.len(), 20);
    let start = game.ext.logistics.last_tick;
    for t in 1..=31 {
        frame(&mut game, start + t);
    }
    let line = include_str!("../../tests/dotnet_sort/vectors.txt").lines().find(|l| l.starts_with("equal20|")).unwrap();
    let order: Vec<usize> = line.rsplit('|').next().unwrap().split(',').map(|i| i.parse().unwrap()).collect();
    assert_eq!(game.ext.logistics.demand, order.iter().map(|i| put_in[*i]).collect::<Vec<_>>());
    assert_ne!(game.ext.logistics.demand, put_in);
}

/// Two demands for one item: the higher priority one gets the task, and _taskPriorities keeps
/// lower ones from taking iron while the higher one still has room for more.
#[test]
fn the_higher_priority_demand_is_served_first() {
    let Some(mut game) = game() else { return };
    build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let low = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let high = build(&mut game, "Container1", [25.0, 2.0, 50.0]);
    let [supply, low, high] = [supply, low, high].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    let item = add(&mut game, "Iron", supply);
    game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
    set_dirty(&mut game, supply);
    for (inv, priority) in [(low, 0), (high, 2)] {
        let l = &mut game.inventories.get_mut(&inv).unwrap().logistic;
        l.demand = vec![iron];
        l.priority = priority;
        set_dirty(&mut game, inv);
    }
    assert_eq!(game.ext.logistics.demand, vec![high, low]);
    let start = game.ext.logistics.last_tick;
    for t in 1..=31 {
        frame(&mut game, start + t);
    }
    let uid = *game.ext.logistics.all_tasks.get(&item).unwrap();
    assert_eq!(game.ext.logistics.tasks[&uid].demand, high);
    assert_eq!(game.ext.logistics.entities[&high].waiting, 1);
    let iron_hash = game.data.groups[iron].hash;
    assert_eq!(game.ext.logistics.priorities[&game.planet_hash()].get(&iron_hash), Some(&2));
}

/// The logistics window's numbers: drones in stations and flying, tasks to supply and to
/// demand, and per group how many inventories offer and ask for it (set by DirtyInventoryLogistics).
#[test]
fn the_logistics_window_counts_drones_tasks_and_groups() {
    use crate::testkit::{join, one, save, send_scene};
    // The test world (a pod to spawn at), with logistics on.
    let mut save = save();
    save.world["logisticsPaused"] = json!(false);
    let Some(mut game) = crate::game::test_game(save) else { return };
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let (station_inv, supply_inv, demand_inv) = (game.wos[&station].linked_inventory, game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
    join(&mut game, 1, "Ann");
    let iron = game.data.groups[game.data.group("Iron").unwrap()].hash;
    add(&mut game, "Iron", supply_inv);
    send_scene(&mut game, 1, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &[json!(supply_inv), json!([]), json!([iron]), json!(0)]);
    send_scene(&mut game, 1, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &[json!(demand_inv), json!([iron]), json!([]), json!(0)]);
    add(&mut game, "Drone1", station_inv);
    add(&mut game, "Drone1", station_inv);
    game.net.out.clear();
    send_scene(&mut game, 1, "LogisticManager.RetrieveSupplyAndDemandGroupsServerRpc", &[]);
    assert_eq!(one(&game, "LogisticManager.RetrieveSupplyAndDemandGroupsClientRpc"), vec![json!([[iron, 1]]), json!([[iron, 1]])]);
    game.net.out.clear();
    send_scene(&mut game, 1, "LogisticManager.RetrieveDronesCountServerRpc", &[]);
    assert_eq!(one(&game, "LogisticManager.RetrieveDronesCountClientRpc"), vec![json!(2), json!(0), json!(0)], "two drones, no task yet");
    // Once the tasks are set (after a second) and a drone flies to the supply.
    let start = game.ext.logistics.last_tick;
    for t in 1..=40 {
        frame(&mut game, start + t);
    }
    game.net.out.clear();
    send_scene(&mut game, 1, "LogisticManager.RetrieveDronesCountServerRpc", &[]);
    assert_eq!(one(&game, "LogisticManager.RetrieveDronesCountClientRpc"), vec![json!(2), json!(0), json!(1)], "one task on its way to supply");
}

/// Grown produce lying in the world (a drone-pickable group) is fetched by a drone for a
/// demand asking for it: it leaves the ground (its network object goes) and is saved in the
/// demand container.
#[test]
fn drones_pick_up_produce_lying_in_the_world() {
    let Some(mut game) = game() else { return };
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let (station_inv, demand_inv) = (game.wos[&station].linked_inventory, game.wos[&demand].linked_inventory);
    let veg = game.data.group("Vegetable0Growable").unwrap();
    let produce = game.create_world_object(veg, 0, true);
    game.drop_on_floor(produce, [20.0, 2.0, 20.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 0);
    assert!(game.pickables.contains(&produce));
    let net = game.wos[&produce].net.unwrap();
    game.inventories.get_mut(&demand_inv).unwrap().logistic.demand = vec![veg];
    set_dirty(&mut game, demand_inv);
    add(&mut game, "Drone1", station_inv);
    let start = game.ext.logistics.last_tick;
    for t in 1..=40 * 30 {
        frame(&mut game, start + t);
        if items(&game, demand_inv).contains(&produce) {
            break;
        }
    }
    assert_eq!(items(&game, demand_inv), vec![produce]);
    assert!(!game.net.objects.contains_key(&net) && game.wos[&produce].net.is_none(), "off the ground");
    assert!(!game.wos[&produce].is_placed());
}

/// A drone carrying iron whose demand container is taken down loses its load, even with
/// another demand for iron there: the game's FindDemandForDroneOrDestroyContent looks for one,
/// but CreateNewTaskForWorldObject refuses a drone's own inventory as the source, so the load is
/// always destroyed.
#[test]
fn a_drone_whose_demand_goes_loses_its_load() {
    for second_demand in [true, false] {
        let Some(mut game) = game() else { return };
        build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
        let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
        let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
        let demand = build(&mut game, "Container1", [25.0, 2.0, 60.0]);
        let (station_inv, supply_inv, demand_inv) = (game.wos[&station].linked_inventory, game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
        let iron = game.data.group("Iron").unwrap();
        let item = add(&mut game, "Iron", supply_inv);
        game.inventories.get_mut(&supply_inv).unwrap().logistic.supply = vec![iron];
        set_dirty(&mut game, supply_inv);
        game.inventories.get_mut(&demand_inv).unwrap().logistic.demand = vec![iron];
        set_dirty(&mut game, demand_inv);
        let drone = add(&mut game, "Drone1", station_inv);
        let start = game.ext.logistics.last_tick;
        let carried = |game: &Game| game.ext.logistics.drones.get(&drone).is_some_and(|d| items(game, d.inventory).contains(&item));
        let mut t = 0;
        while !carried(&game) {
            t += 1;
            assert!(t < 40 * 30, "the drone never took the iron");
            frame(&mut game, start + t);
        }
        let other = second_demand.then(|| {
            let other = build(&mut game, "Container1", [30.0, 2.0, 20.0]);
            let inv = game.wos[&other].linked_inventory;
            game.inventories.get_mut(&inv).unwrap().logistic.demand = vec![iron];
            set_dirty(&mut game, inv);
            inv
        });
        game.destroy_world_object(demand, true);
        for _ in 0..40 * 30 {
            t += 1;
            frame(&mut game, start + t);
        }
        if let Some(inv) = other {
            assert!(items(&game, inv).is_empty());
        }
        assert!(!game.wos.contains_key(&item), "destroyed");
        assert!(items(&game, game.ext.logistics.drones[&drone].inventory).is_empty());
    }
}

/// A drone item dropped on the floor is a drone (Drone.Awake); picked up again, its GameObject
/// goes and Drone.OnDestroy with it: no drone left in the logistics, and no second destroy for
/// its network object (sweep_items_2, Drone1|pick up).
#[test]
fn a_drone_picked_up_from_the_floor_is_no_drone_any_more() {
    let Some(mut game) = game() else { return };
    let g = game.data.group("Drone1").unwrap();
    let drone = game.create_world_object(g, 0, true);
    game.drop_on_floor(drone, [20.0, 2.0, 20.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 0);
    assert!(game.ext.logistics.drones.contains_key(&drone));
    let bag = game.create_inventory(10, 0, 0, &[], &[]);
    game.net.out.clear();
    game.inventories_rpc(1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &crate::game::Args(vec![json!(drone), json!(game.data.groups[g].hash), json!(bag), json!(true)]));
    assert!(!game.ext.logistics.drones.contains_key(&drone));
    let start = game.ext.logistics.last_tick;
    for t in 1..=60 {
        frame(&mut game, start + t);
    }
    let destroys = game.net.out.iter().filter(|(_, kind, _)| *kind == DESTROY_OBJECT).count();
    assert_eq!(destroys, 1);
}

/// The oracle's errand with the game's clock begun `days` into the server's life: the frames run at
/// the tick rate whatever the age (the frame loop counts ticks in 64 bits; the game's int tick, on
/// the wire, wraps at 828 days), and the drone brings both iron home in the same ticks as on day 0.
#[test]
fn drones_fly_at_the_same_pace_at_any_age_of_the_server() {
    let mut rows = Vec::new();
    for days in [0.0, 3.0, 60.0, 400.0, 900.0, 5000.0] {
        let Some((net_data, data, spec, scenes)) = crate::game::test_tables() else { return };
        let mut save = SaveFile::empty();
        save.settings = json!({"planetId": "Prime"});
        save.world = json!({"logisticsPaused": false});
        save.planets = vec![json!({"planetId": "Prime"})];
        let start = days * 86_400.0;
        let mut game = Game::with_clock(net_data, data, spec, save, scenes, Some(start));
        build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
        let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
        let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
        let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
        let (station_inv, supply_inv, demand_inv) = (game.wos[&station].linked_inventory, game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
        let iron = game.data.group("Iron").unwrap();
        add(&mut game, "Iron", supply_inv);
        add(&mut game, "Iron", supply_inv);
        game.inventories.get_mut(&supply_inv).unwrap().logistic.supply = vec![iron];
        set_dirty(&mut game, supply_inv);
        game.inventories.get_mut(&demand_inv).unwrap().logistic.demand = vec![iron];
        set_dirty(&mut game, demand_inv);
        add(&mut game, "Drone1", station_inv);
        game.net.out.clear();
        let (mut transforms, mut delivered, mut frames) = (0, None, 0u64);
        let first = game.tick_count();
        // Ticks 17.1 ms apart, a pace no timer of the game divides.
        for k in 1..=2400u32 {
            game.sim_time = Some(start + f64::from(k) * 0.0171);
            game.tick();
            transforms += game.net.out.iter().filter(|m| m.1 == NETWORK_TRANSFORM).count();
            game.net.out.clear();
            if delivered.is_none() && group_items(&game, demand_inv, "Iron") == 2 {
                delivered = Some(k);
            }
        }
        frames += game.tick_count() - first;
        assert_eq!(game.tick_number(), game.tick_count() as u32 as i32, "the int of the game is the low 32 bits");
        // Past 2^31 ticks (828 days) the int is negative, until the next 2^31.
        if days == 400.0 || days == 900.0 {
            assert_eq!(game.tick_number() < 0, days == 900.0, "{days} days: tick {}", game.tick_number());
        }
        rows.push((days, transforms, delivered, frames));
    }
    let (_, transforms, delivered, frames) = rows[0];
    assert!(delivered.is_some() && transforms > 50, "the errand was run at day 0: {rows:?}");
    for (days, t, d, f) in &rows {
        assert!(d.zip(delivered).is_some_and(|(a, b)| a.abs_diff(b) <= 2), "{days} days: delivered at tick {d:?}, day 0 at {delivered:?}: {rows:?}");
        assert!(t.abs_diff(transforms) <= 2, "{days} days: {t} transforms against {transforms}: {rows:?}");
        assert!(f.abs_diff(frames) <= 1, "{days} days: {f} frames against {frames}: {rows:?}");
    }
}
