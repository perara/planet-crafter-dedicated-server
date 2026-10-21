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
/// always destroyed. With drones.keepCargo it is dropped on the ground instead.
fn demand_goes(keep: bool) {
    for second_demand in [true, false] {
        let Some(mut game) = game() else { return };
        game.rules.drones.keep_cargo = keep;
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
        if keep {
            if let Some(inv) = other {
                assert!(items(&game, inv).contains(&item) || game.wos[&item].is_placed());
            }
            assert!(game.wos.contains_key(&item), "cancelled delivery must preserve the cargo");
            assert!(!game.wos[&item].dont_save);
        } else {
            if let Some(inv) = other {
                assert!(items(&game, inv).is_empty());
            }
            assert!(!game.wos.contains_key(&item), "destroyed");
        }
        assert!(items(&game, game.ext.logistics.drones[&drone].inventory).is_empty());
    }
}

#[test]
fn a_drone_whose_demand_goes_loses_its_load() {
    demand_goes(false);
}

#[test]
fn a_drone_whose_demand_goes_keeps_its_load_with_the_rule() {
    demand_goes(true);
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
        // The ids of the drone and the containers come from the generator: the same at every age.
        game.rng = crate::game::Rng::fixed(1);
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

/// A drone farm that runs for hours of game time (an ignored soak, not a check): PCDS_FARM_STATIONS stations of
/// PCDS_FARM_DRONES drones each serve PCDS_FARM_PAIRS supply and demand containers; supply is
/// topped up and demand emptied every game minute. Prints, per game minute, the worst tick and
/// the sizes of what could grow (world objects, network objects, tasks, the fleet) and the
/// process's memory.
/// PCDS_FARM_SECONDS=3600 cargo test --release drone_farm -- --ignored --nocapture
#[test]
#[ignore]
fn drone_farm_runs_for_hours() {
    let Some(mut game) = game() else { return };
    let env = |k: &str, d: usize| crate::env(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (stations, drones, pairs, seconds) = (env("PCDS_FARM_STATIONS", 5), env("PCDS_FARM_DRONES", 4), env("PCDS_FARM_PAIRS", 20), env("PCDS_FARM_SECONDS", 3600));
    let topup = 60;
    for i in 0..4 + stations * 2 {
        build(&mut game, "EnergyGenerator6", [60.0 + 10.0 * i as f32, 2.0, 0.0]);
    }
    let iron = game.data.group("Iron").unwrap();
    let mut supplies = Vec::new();
    let mut demands = Vec::new();
    for s in 0..stations {
        let station = build(&mut game, "DroneStation1", [10.0 + 15.0 * s as f32, 2.0, 10.0]);
        let inv = game.wos[&station].linked_inventory;
        for _ in 0..drones {
            add(&mut game, "Drone1", inv);
        }
    }
    for p in 0..pairs {
        let supply = build(&mut game, "Container1", [20.0 + 6.0 * p as f32, 2.0, 40.0]);
        let demand = build(&mut game, "Container1", [20.0 + 6.0 * p as f32, 2.0, 90.0]);
        let (si, di) = (game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
        game.inventories.get_mut(&si).unwrap().logistic.supply = vec![iron];
        set_dirty(&mut game, si);
        game.inventories.get_mut(&di).unwrap().logistic.demand = vec![iron];
        set_dirty(&mut game, di);
        supplies.push(si);
        demands.push(di);
    }
    assert!(crate::energy::has_enough(&game, game.planet_hash()));
    let rss = || std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("VmRSS")).map(|l| l.split_whitespace().nth(1).unwrap().to_string()).unwrap_or_default();
    let benchmark_start = std::time::Instant::now();
    let cold = std::time::Instant::now();
    refresh_navigation(&mut game);
    let cold = cold.elapsed();
    build(&mut game,"Container1",[5000.0,2.0,5000.0]);
    let changed = std::time::Instant::now();
    refresh_navigation(&mut game);
    println!("navigation index: cold {:.3} ms; after one new building {:.3} ms",cold.as_secs_f64()*1000.0,changed.elapsed().as_secs_f64()*1000.0);
    let (mut t, mut worst, mut all_worst, mut delivered) = (2.0, 0.0f64, 0.0f64, 0usize);
    game.sim_time = Some(t);
    game.tick();
    for i in 0..seconds * 200 {
        t += 0.005;
        game.sim_time = Some(t);
        let t0 = std::time::Instant::now();
        game.tick();
        let took = t0.elapsed().as_secs_f64();
        worst = worst.max(took);
        game.net.out.clear();
        if i % (topup * 200) == topup * 200 - 1 {
            for (si, di) in supplies.iter().zip(&demands) {
                while items(&game, *si).len() < 3 {
                    add(&mut game, "Iron", *si);
                }
                let got: Vec<i32> = items(&game, *di);
                delivered += got.len();
                game.destroy_world_objects(&got, false);
            }
        }
        if i % (60 * 200) == 60 * 200 - 1 {
            all_worst = all_worst.max(worst);
            println!("minute {}: worst tick {:.1} ms; wos {} net {} inventories {} fleet {} tasks {} delivered so far {delivered}; rss {} kB", (i + 1) / 12000, worst * 1000.0, game.wos.len(), game.net.objects.len(), game.inventories.len(), game.ext.logistics.fleet.len(), game.ext.logistics.all_tasks.iter().count(), rss());
            worst = 0.0;
        }
    }
    println!("{seconds} s of game, {stations} stations x {drones} drones, {pairs} pairs: worst tick {:.1} ms, {delivered} items delivered, {:.3} s wall time", all_worst * 1000.0, benchmark_start.elapsed().as_secs_f64());
}

/// The search for tasks as the port first had it (every demand through every supply and every
/// dropped item), kept as the oracle of the one that went through the items left.
fn reference_set_logistic_tasks(game: &mut Game) {
    game.ext.logistics.updating = 1;
    let pickables: Vec<i32> = game.pickables.iter().copied().collect();
    for p in game.ext.logistics.priorities.values_mut() {
        p.clear();
    }
    let mut i = 0;
    while i < game.ext.logistics.demand.len() {
        let d = game.ext.logistics.demand[i];
        i += 1;
        let Some(inv) = game.inventories.get(&d) else { continue };
        let waiting = game.ext.logistics.entities.get(&d).map(|e| e.waiting).unwrap_or(0);
        let mut available = (inv.size - inv.items.len() as i32 - waiting).max(0);
        let (full, priority, groups) = (inv.is_full(), inv.logistic.priority, inv.logistic.demand.clone());
        let planet = entity_planet(game, d);
        if full || available == 0 || entity_wo(game, d).is_none() || !game.ext.logistics.priorities.contains_key(&planet) {
            continue;
        }
        for &g in &groups {
            let hash = game.data.groups[g].hash;
            if game.ext.logistics.priorities[&planet].get(&hash).is_some_and(|v| priority < *v) {
                continue;
            }
            for s in game.ext.logistics.supply.clone() {
                let supplies = game.inventories.get(&s).is_some_and(|i| i.logistic.supply.contains(&g));
                if s == d || entity_wo(game, s).is_none() || entity_planet(game, s) != planet || !supplies {
                    continue;
                }
                for wo in items(game, s) {
                    if game.wos.get(&wo).is_some_and(|w| w.group == g) && create_task(game, Some(s), d, wo, false).is_some() {
                        available -= 1;
                        if available == 0 {
                            break;
                        }
                    }
                }
                if available == 0 {
                    break;
                }
            }
            if available == 0 {
                continue;
            }
            for &wo in &pickables {
                // A task for it already: create_task refuses it, so its checks can wait.
                if game.ext.logistics.all_tasks.contains_key(&wo) {
                    continue;
                }
                let ok = game.wos.get(&wo).is_some_and(|w| w.group == g && w.net.is_some() && motion::length_sq(w.position) >= 1e-10 && can_grab(game, w) && w.planet_hash() == planet);
                if ok && create_task(game, None, d, wo, true).is_some() {
                    available -= 1;
                    if available == 0 {
                        break;
                    }
                }
            }
        }
        if available > 0 {
            let hashes: Vec<i32> = groups.iter().map(|g| game.data.groups[*g].hash).collect();
            if let Some(p) = game.ext.logistics.priorities.get_mut(&planet) {
                for h in hashes {
                    let e = p.entry(h).or_insert(priority);
                    *e = (*e).max(priority);
                }
            }
        }
    }
    let entries: Vec<(i32, u32)> = game.ext.logistics.all_tasks.iter().map(|(k, v)| (*k, *v)).collect();
    let mut over = Vec::new();
    for (wo, uid) in entries {
        let t = &game.ext.logistics.tasks[&uid];
        if is_full(game, t.demand) {
            set_state(game, uid, State::Done);
            over.push(wo);
        } else if t.state == State::Done {
            over.push(wo);
        }
    }
    let l = &mut game.ext.logistics;
    for wo in over {
        l.all_tasks.remove(&wo);
    }
    let held: HashSet<u32> = l.all_tasks.iter().map(|(_, uid)| *uid).chain(l.drones.values().filter_map(|d| d.task)).collect();
    l.tasks.retain(|uid, _| held.contains(uid));
}


/// A base of containers that supply and demand in many ways (demands of several groups, so that
/// one group takes the last room and the next is met with none; supplies that are also in
/// the way of their own demands; items on the ground; full inventories) from a seed.
fn tasks_world(seed: u64) -> Option<Game> {
    let mut game = game()?;
    // The ids the game picks at random are the same in both worlds of a seed.
    game.rng = crate::game::Rng::fixed(seed);
    let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut roll = move |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let names = ["Iron", "Magnesium", "Silicon", "Aluminium", "Cobalt"];
    let groups: Vec<usize> = names.iter().map(|n| game.data.group(n).unwrap()).collect();
    for i in 0..30 {
        let wo = build(&mut game, "Container1", [25.0 + (i % 6) as f32 * 3.0, 2.0, 10.0 + (i / 6) as f32 * 3.0]);
        let inventory = game.wos[&wo].linked_inventory;
        for _ in 0..roll(9) {
            add(&mut game, names[roll(5) as usize], inventory);
        }
        let pick = |roll: &mut dyn FnMut(u64) -> u64| -> Vec<usize> { (0..roll(4)).map(|_| groups[roll(5) as usize]).collect() };
        let (supply, demand) = (pick(&mut roll), pick(&mut roll));
        let inv = game.inventories.get_mut(&inventory).unwrap();
        inv.logistic.supply = supply;
        inv.logistic.demand = demand;
        inv.logistic.priority = roll(3) as i32;
        set_dirty(&mut game, inventory);
    }
    for _ in 0..40 {
        game.create_and_drop_on_floor(groups[roll(5) as usize], [30.0 + roll(20) as f32, 2.0, 12.0 + roll(20) as f32], 0.6, true, false, 0.0, 0);
    }
    Some(game)
}

/// What tasks_coroutine does before a search, for either search.
fn ready_for_search(game: &mut Game) {
    let ids: Vec<i32> = game.inventories.values().filter(|i| !i.logistic.demand.is_empty() || !i.logistic.supply.is_empty()).map(|i| i.id).collect();
    for id in ids {
        set_inventory_status(game, id);
    }
    let mut demand = std::mem::take(&mut game.ext.logistics.demand);
    let priority = |i: &i32| game.inventories.get(i).map(|i| i.logistic.priority).unwrap_or(0);
    let _ = crate::dotnet_sort::sort_by(&mut demand, |x, y| priority(y).cmp(&priority(x)));
    game.ext.logistics.demand = demand;
    game.ext.logistics.inventories_inited = true;
}

#[test]
fn the_search_for_tasks_makes_the_tasks_the_first_search_made_in_the_same_order() {
    let mut made = 0;
    for seed in 1..=40 {
        let (Some(mut new), Some(mut old)) = (tasks_world(seed), tasks_world(seed)) else { return };
        for game in [&mut new, &mut old] {
            ready_for_search(game);
        }
        // Several searches in a row: tasks stay between them, as they do live.
        for round in 0..3 {
            set_logistic_tasks(&mut new);
            reference_set_logistic_tasks(&mut old);
            let summary = |g: &Game| -> Vec<(i32, u32)> { g.ext.logistics.all_tasks.iter().map(|(wo, uid)| (*wo, *uid)).collect() };
            assert_eq!(summary(&new), summary(&old), "seed {seed}, search {round}");
            let tasks = |g: &Game| -> Vec<(u32, i32, Option<i32>, i32, bool)> {
                let mut v: Vec<_> = g.ext.logistics.tasks.iter().map(|(uid, t)| (*uid, t.wo, t.supply, t.demand, t.spawned)).collect();
                v.sort();
                v
            };
            assert_eq!(tasks(&new), tasks(&old), "seed {seed}, search {round}");
            let waiting = |g: &Game| -> Vec<(i32, i32)> {
                let mut v: Vec<_> = g.ext.logistics.entities.iter().map(|(i, e)| (*i, e.waiting)).collect();
                v.sort();
                v
            };
            assert_eq!(waiting(&new), waiting(&old), "seed {seed}, search {round}");
            assert_eq!(new.ext.logistics.next_task, old.ext.logistics.next_task);
            made += new.ext.logistics.all_tasks.iter().count();
            // Some deliveries between searches: items leave the supplies' stock for new ones.
            for game in [&mut new, &mut old] {
                let wos: Vec<i32> = game.ext.logistics.all_tasks.iter().map(|(wo, _)| *wo).step_by(3).collect();
                for wo in wos {
                    if let Some(uid) = game.ext.logistics.all_tasks.get(&wo).copied() {
                        set_state(game, uid, State::Done);
                    }
                }
            }
        }
    }
    assert!(made > 500, "the scenarios made {made} tasks: they should exercise the search");
}

#[test]
fn a_drone_preserves_cargo_if_destination_fills_during_flight() {
    let Some(mut game) = game() else { return };
    game.rules.drones.keep_cargo = true;
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let [station, supply, demand] = [station, supply, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    let cargo = add(&mut game, "Iron", supply);
    game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
    game.inventories.get_mut(&demand).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, supply); set_dirty(&mut game, demand);
    let drone = add(&mut game, "Drone1", station);
    let start = game.ext.logistics.last_tick;
    let mut filled = false;
    for t in 1..=40*30 {
        frame(&mut game, start+t);
        if !filled && game.ext.logistics.drones.get(&drone).is_some_and(|d| items(&game, d.inventory).contains(&cargo)) {
            filled = true;
            let capacity = game.inventories[&demand].size;
            for _ in 0..capacity { add(&mut game, "ice", demand); }
            assert!(game.inventories[&demand].is_full());
        }
    }
    assert!(filled, "control: the drone picked up the iron before the destination filled");
    assert!(game.wos.contains_key(&cargo), "iron was deleted after the destination filled during delivery");
    assert!(game.wos[&cargo].net.is_some() && game.wos[&cargo].is_placed(), "undelivered cargo is dropped for recovery");
    assert!(!game.is_held(cargo));
    assert!(items(&game, station).contains(&drone), "drone still returns home");
}

#[test]
fn cancelled_drone_cargo_is_saved_on_its_own_planet() {
    let Some(mut g) = game() else { return };
    g.rules.drones.keep_cargo = true;
    let drone = build(&mut g, "Drone1", [40.0, 6.0, 30.0]);
    add_to_fleet(&mut g, drone, false);
    let inventory = g.ext.logistics.drones[&drone].inventory;
    let cargo = add(&mut g, "Iron", inventory);
    let planet = stable_hash("Humble");
    g.wos.get_mut(&drone).unwrap().planet = planet;
    let position = g.ext.logistics.drones[&drone].position;
    end_assignment(&mut g, drone);
    assert!(items(&g, inventory).is_empty());
    assert!(!g.is_held(cargo));
    assert_eq!(g.wos[&cargo].planet_hash(), planet);
    assert_eq!(g.wos[&cargo].position, position);
    assert!(!g.wos[&cargo].dont_save);
    let net = g.wos[&cargo].net.unwrap();
    assert!(matches!(g.net.get(net, "GroupNetworkContainer", "_active"), Some(crate::net::VarValue::Plain(v)) if *v == enc::bool(false)));
}

#[test]
fn supplies_are_chosen_nearest_the_destination() {
    let Some(mut game) = game() else { return };
    game.rules.drones.nearest_first = true;
    build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let far = build(&mut game, "Container1", [1000.0, 2.0, 20.0]);
    let near = build(&mut game, "Container1", [30.0, 2.0, 20.0]);
    let demand = build(&mut game, "Container1", [35.0, 2.0, 20.0]);
    let [far, near, demand] = [far, near, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    for supply in [far, near] {
        add(&mut game, "Iron", supply);
        game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
        set_dirty(&mut game, supply);
    }
    game.inventories.get_mut(&demand).unwrap().size = 1;
    game.inventories.get_mut(&demand).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, demand);
    set_logistic_tasks(&mut game);
    let tasks: Vec<_> = game.ext.logistics.all_tasks.iter().map(|(_, u)| &game.ext.logistics.tasks[u]).collect();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].supply, Some(near));
    println!("loaded leg: 5 m selected, versus 965 m from the first registered supply");
}

/// A live airborne drone plus tasks deliberately ordered far before near.
fn dispatch_scenario(game: &mut Game) -> (i32, u32, u32) {
    let far = build(game, "Container1", [1000.0, 2.0, 10.0]);
    let near = build(game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(game, "Container1", [40.0, 2.0, 10.0]);
    let [far, near, demand] = [far, near, demand].map(|w| game.wos[&w].linked_inventory);
    for i in [far, near, demand] {
        set_dirty(game, i);
    }
    let far_item = add(game, "Iron", far);
    let near_item = add(game, "Iron", near);
    let far_task = create_task(game, Some(far), demand, far_item, false).unwrap();
    let near_task = create_task(game, Some(near), demand, near_item, false).unwrap();
    let drone = build(game, "Drone1", [10.0, 4.0, 10.0]);
    add_to_fleet(game, drone, false);
    (drone, far_task, near_task)
}

#[test]
fn idle_drones_choose_nearby_work_and_preserve_priority_pause_and_reservations() {
    let Some(mut game) = game() else { return };
    game.rules.drones.nearest_first = true;
    let (drone, far, near) = dispatch_scenario(&mut game);
    assign_idle_drones(&mut game);
    assert_eq!(drone_task(&game, drone), Some(near));
    assert_eq!(task_state(&game, near), Some(State::ToSupply));
    assert_eq!(task_state(&game, far), Some(State::NotAttributed));
    // A second drone cannot take an already reserved item.
    let second = build(&mut game, "Drone1", [10.0, 4.0, 10.0]);
    add_to_fleet(&mut game, second, false);
    assert_eq!(next_task_for_drone(&game, second), Some(far));
    let high = build(&mut game, "Container1", [1100.0, 2.0, 10.0]);
    let high = game.wos[&high].linked_inventory;
    game.inventories.get_mut(&high).unwrap().logistic.priority = 2;
    set_dirty(&mut game, high);
    let source = game.ext.logistics.tasks[&far].supply;
    let other = add(&mut game, "Iron", source.unwrap());
    let high_task = create_task(&mut game, source, high, other, false).unwrap();
    assert_eq!(next_task_for_drone(&game, second), Some(high_task));
    game.ext.logistics.paused = true;
    assert_eq!(next_task_for_drone(&game, second), None);
}

#[test]
fn parked_drones_choose_the_nearest_job_before_the_first_registered_job() {
    let Some(mut game) = game() else { return };
    game.rules.drones.nearest_first = true;
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let (airborne, far, near) = dispatch_scenario(&mut game);
    game.destroy_world_object(airborne, true);
    let inventory = game.wos[&station].linked_inventory;
    let drone = add(&mut game, "Drone1", inventory);
    release_drones(&mut game);
    assert_eq!(drone_task(&game, drone), Some(near), "a parked drone must also choose the nearby pickup");
    assert_eq!(task_state(&game, far), Some(State::NotAttributed));
}

#[test]
fn parked_dispatch_keeps_priority_and_reconsiders_distance_when_a_station_empties() {
    for priority in [false, true] {
        let Some(mut game) = game() else { return };
        game.rules.drones.nearest_first = true;
        let mut drones = Vec::new();
        for x in [10.0, 110.0] {
            let station = build(&mut game, "DroneStation1", [x, 2.0, 10.0]);
            let inventory = game.wos[&station].linked_inventory;
            drones.push(add(&mut game, "Drone1", inventory));
        }
        let mut tasks = Vec::new();
        for (i, x) in [20.0, 25.0, 80.0].into_iter().enumerate() {
            let supply = build(&mut game, "Container1", [x, 2.0, 10.0]);
            let demand = build(&mut game, "Container1", [200.0 + i as f32 * 10.0, 2.0, 10.0]);
            let (supply, demand) = (game.wos[&supply].linked_inventory, game.wos[&demand].linked_inventory);
            if priority && i == 1 { game.inventories.get_mut(&demand).unwrap().logistic.priority = 2; }
            for inv in [supply, demand] { set_dirty(&mut game, inv); }
            let item = add(&mut game, "Iron", supply);
            tasks.push(create_task(&mut game, Some(supply), demand, item, false).unwrap());
        }
        game.ext.logistics.paused = true;
        release_drones(&mut game);
        assert!(drones.iter().all(|d| drone_task(&game, *d).is_none()));
        game.ext.logistics.paused = false;
        release_drones(&mut game);
        let first = usize::from(priority);
        assert_eq!(drone_task(&game, drones[0]), Some(tasks[first]), "priority outranks pickup distance");
        assert_eq!(drone_task(&game, drones[1]), Some(tasks[2]), "the second station must choose again after the first empties");
        assert_eq!(task_state(&game, tasks[1 - first]), Some(State::NotAttributed));
    }
}

#[test]
fn building_only_detours_are_flown_and_sent_to_the_client() {
    use crate::geometry::navigation::{Bounds, Data};
    let Some((mut game, _)) = crate::testkit::world() else { return };
    game.rules.drones.avoid_obstacles = true;
    build(&mut game, "EnergyGenerator6", [10.0, 2.0, 0.0]);
    build(&mut game, "Container1", [45.0, 2.0, 10.0]);
    let planet = game.planet_hash();
    game.data.drone_navigation = std::sync::Arc::new(crate::gamedata::DroneMap::of(Data {
        planets: HashMap::from([(planet, Vec::new().into())]),
        buildings: HashMap::from([("Container1".into(), vec![Bounds {
            min: [-3.0, 0.0, -6.0], max: [3.0, 30.0, 6.0],
        }])]),
        ..Default::default()
    }));
    let drone = build(&mut game, "Drone1", [10.0, 4.0, 10.0]);
    add_to_fleet(&mut game, drone, false);
    set_target(&mut game, drone, [80.0, 2.0, 10.0]);
    game.ext.logistics.drones.get_mut(&drone).unwrap().distance_min = 0.01;
    game.ext.logistics.min_target_distance = 0.01;
    game.ext.logistics.enabled = true;
    let net = game.ext.logistics.drones[&drone].net.unwrap();
    let mut client_position = [10.0, 4.0, 10.0];
    let (mut server_crossed, mut client_crossed) = (false, false);
    for tick in 0..1200 {
        update(&mut game);
        send_transforms(&mut game, tick);
        let position = game.ext.logistics.drones[&drone].position;
        if (42.0..=48.0).contains(&position[0]) {
            assert!(position[1] > 35.0, "server crossed the building: {position:?}");
            server_crossed = true;
        }
        for (_, kind, body) in std::mem::take(&mut game.net.out) {
            if kind != NETWORK_TRANSFORM { continue; }
            let mut r = Reader::new(&body);
            if r.packed_u64() != Some(net) { continue; }
            r.packed_i32().unwrap(); // behaviour
            let flags = r.packed_u32().unwrap();
            r.packed_i32().unwrap(); // tick
            for (i, coordinate) in client_position.iter_mut().enumerate() {
                if flags & (2 << i) != 0 { *coordinate = r.f32().unwrap(); }
            }
            if (42.0..=48.0).contains(&client_position[0]) {
                assert!(client_position[1] > 32.0, "client was sent a flight through the building: {client_position:?}");
                client_crossed = true;
            }
        }
        if motion::distance(position, [80.0, 4.0, 10.0]) < 0.1 { break; }
    }
    assert!(server_crossed && client_crossed, "the detour must reach both server movement and client packets");
    assert!(motion::distance(game.ext.logistics.drones[&drone].position, [80.0, 4.0, 10.0]) < 0.1);
}

#[test]
#[ignore = "requires PCDS_BENCH_SAVE and the installed game's collision map"]
fn deliveries_on_a_downloaded_world_clear_the_real_map() {
    use crate::geometry::navigation::Bounds;
    let text = std::fs::read_to_string(std::env::var("PCDS_BENCH_SAVE").expect("a private save copy required")).unwrap();
    let (net, data, spec, scenes) = crate::game::test_tables().expect("game tables required");
    let imported = crate::oldsave::import(&text, 1, &net.game_version).unwrap();
    let mut game = Game::with_clock(net, data, spec, SaveFile::from_text(&imported), scenes, Some(0.0));
    game.rules.drones = crate::rules::Drones { nearest_first: true, avoid_obstacles: true, keep_cargo: true };
    let map = game.data.drone_navigation.get().expect("extracted collision map required");
    build(&mut game, "EnergyGenerator6", [560.0, 20.0, 600.0]);
    let station = build(&mut game, "DroneStation1", [580.0, 20.0, 600.0]);
    let supply = build(&mut game, "Container1", [600.0, 20.0, 600.0]);
    let demand = build(&mut game, "Container1", [600.0, 20.0, 680.0]);
    let [station, supply, demand] = [station, supply, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    let cargo = add(&mut game, "Iron", supply);
    let drone = add(&mut game, "Drone1", station);
    game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
    game.inventories.get_mut(&demand).unwrap().logistic.demand = vec![iron];
    for inv in [supply, demand] { set_dirty(&mut game, inv); }
    let terrain = map.planets[&game.planet_hash()].clone();
    let buildings: Vec<_> = game.constructed.iter().filter_map(|id| game.wos.get(id)).filter(|w| w.planet_hash() == game.planet_hash())
        .flat_map(|w| map.buildings.get(&game.data.groups[w.group].id).into_iter().flatten().map(move |b| b.transformed(w.position, w.rotation))).collect();
    // Independent double-precision physical-bounds oracle, without the route planner's grid.
    let hits = |b: &Bounds, a: V3, z: V3| {
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for i in 0..3 {
            let delta = f64::from(z[i]) - f64::from(a[i]);
            if delta == 0.0 {
                if a[i] < b.min[i] || a[i] > b.max[i] { return false; }
            } else {
                let (p, q) = ((f64::from(b.min[i]) - f64::from(a[i])) / delta, (f64::from(b.max[i]) - f64::from(a[i])) / delta);
                lo = lo.max(p.min(q));
                hi = hi.min(p.max(q));
                if lo > hi { return false; }
            }
        }
        true
    };
    let mut last = None;
    let mut checked = 0;
    for tick in 1..=60_000 {
        game.sim_time = Some(tick as f64 * 0.005);
        game.tick();
        if let Some(d) = game.ext.logistics.drones.get(&drone) {
            let key = (d.route_goal, d.route_epoch);
            if d.route_goal.is_some() && last != Some(key) {
                last = Some(key);
                let mut previous = d.position;
                for &next in &d.route {
                    // The allowed vertical pickup/docking shafts may cross walls.
                    if (previous[0] - next[0]).abs() + (previous[2] - next[2]).abs() > 0.001 {
                        assert!(!terrain.iter().chain(&buildings).any(|b| hits(b, previous, next)), "flight {previous:?} -> {next:?} crosses a real collider");
                        checked += 1;
                    }
                    previous = next;
                }
                println!("real-world drone route: {:?}", d.route);
            }
        }
        game.net.out.clear();
        if items(&game, demand).contains(&cargo) && items(&game, station).contains(&drone) { break; }
    }
    assert!(checked >= 2, "must check real flights, not an idle world");
    assert!(items(&game, demand).contains(&cargo), "cargo never arrived");
    assert!(items(&game, station).contains(&drone), "drone never returned to its station");
    println!("downloaded world: {checked} cruise segments cleared all {} terrain and {} constructed-building bounds; cargo delivered and drone returned", terrain.len(), buildings.len());
}

#[test]
fn an_idle_drone_on_another_planet_does_not_stop_dispatch() {
    let Some(mut game) = game() else { return };
    game.rules.drones.nearest_first = true;
    let (first, _, _) = dispatch_scenario(&mut game);
    // This drone's planet has no work. The later access slot still must be considered.
    game.wos.get_mut(&first).unwrap().planet = stable_hash("Humble");
    let second = build(&mut game, "Drone1", [10.0, 4.0, 10.0]);
    add_to_fleet(&mut game, second, false);
    assign_idle_drones(&mut game);
    assert_eq!(drone_task(&game, first), None);
    assert!(drone_task(&game, second).is_some());
}

#[test]
fn clearance_routes_deliver_cargo_and_rebuild_after_construction_and_snapshot() {
    use crate::geometry::navigation::{Bounds, Data};
    let Some(mut game) = game() else { return };
    game.rules.drones.avoid_obstacles = true;
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 60.0]);
    let [station_inv, supply_inv, demand_inv] = [station, supply, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    let cargo = add(&mut game, "Iron", supply_inv);
    game.inventories.get_mut(&supply_inv).unwrap().logistic.supply = vec![iron];
    game.inventories.get_mut(&demand_inv).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, supply_inv);
    set_dirty(&mut game, demand_inv);
    let drone = add(&mut game, "Drone1", station_inv);
    let planet = game.planet_hash();
    let map = Data {
        format: 1,
        build_id: "test".into(),
        planets: HashMap::from([(
            planet,
            vec![Bounds {
                min: [20.0, 0.0, 30.0],
                max: [30.0, 20.0, 40.0],
            }].into(),
        )]),
        buildings: HashMap::from([(
            "Container1".into(),
            vec![Bounds {
                min: [-2.0, 0.0, -2.0],
                max: [2.0, 3.0, 2.0],
            }],
        )]),
    };
    game.data.drone_navigation = std::sync::Arc::new(crate::gamedata::DroneMap::of(map));
    let start = game.ext.logistics.last_tick;
    let mut checked_cruise = false;
    for tick in 1..=80 * 30 {
        frame(&mut game, start + tick);
        if let Some(d) = game.ext.logistics.drones.get(&drone) {
            assert!(d.position.iter().chain(&d.rotation).all(|v| v.is_finite()));
            if d.position[2] > 27.0 && d.position[2] < 43.0 && (d.position[0] - 25.0).abs() < 3.0 {
                assert!(d.position[1] >= 23.0, "cruise crossed the terrain: {:?}", d.position);
                checked_cruise = true;
            }
        }
        game.net.out.clear();
    }
    assert!(checked_cruise);
    assert!(items(&game, demand_inv).contains(&cargo));
    assert!(items(&game, station_inv).contains(&drone));
    let epoch = game.ext.logistics.navigation_epoch;
    let terrain = &game.ext.logistics.terrain_navigation[&planet] as *const _;
    let building = build(&mut game, "Container1", [50.0, 2.0, 40.0]);
    refresh_navigation(&mut game);
    assert_eq!(game.ext.logistics.navigation_epoch, epoch + 1);
    assert_eq!(&game.ext.logistics.terrain_navigation[&planet] as *const _,terrain,"construction reuses the static terrain index");
    refresh_navigation(&mut game);
    assert_eq!(game.ext.logistics.navigation_epoch, epoch + 1, "unchanged world reuses the index");
    game.destroy_world_objects(&[building], false);
    refresh_navigation(&mut game);
    assert_eq!(game.ext.logistics.navigation_epoch, epoch + 2);
    let snapshot = crate::handoff::snapshot(&mut game).unwrap();
    let (_, _, spec) = crate::tables(crate::test_data().unwrap());
    let mut restored = crate::handoff::restore(game.net_data.clone(), game.data.clone(), spec, &snapshot, game.scenes.clone()).unwrap();
    assert!(restored.ext.logistics.navigation.is_empty());
    refresh_navigation(&mut restored);
    assert!(!restored.ext.logistics.navigation.is_empty());
    assert!(items(&restored, demand_inv).contains(&cargo));
}

#[test]
fn mixed_drone_types_use_their_own_arrival_distance() {
    let Some(mut game) = game() else { return };
    game.rules.drones.nearest_first = true;
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let a = build(&mut game, "Drone1", [10.0, 4.0, 10.0]);
    let b = build(&mut game, "Drone1", [20.0, 4.0, 10.0]);
    for d in [a, b] {
        add_to_fleet(&mut game, d, false);
    }
    game.ext.logistics.enabled = true;
    game.ext.logistics.drones.get_mut(&a).unwrap().distance_min = 3.0;
    game.ext.logistics.drones.get_mut(&b).unwrap().distance_min = 0.5;
    game.ext.logistics.min_target_distance = 0.1;
    set_target(&mut game, a, [12.0, 2.0, 10.0]);
    set_target(&mut game, b, [22.0, 2.0, 10.0]);
    update(&mut game);
    assert!(game.ext.logistics.at_target.contains(&(game.ext.logistics.drones[&a].access as usize)));
    assert!(!game.ext.logistics.at_target.contains(&(game.ext.logistics.drones[&b].access as usize)));
}

#[test]
fn route_planning_is_bounded_and_fair_across_the_fleet() {
    use crate::geometry::navigation::Data;
    let Some(mut game) = game() else { return };
    game.rules.drones.avoid_obstacles = true;
    build(&mut game,"EnergyGenerator6",[40.0,2.0,0.0]);
    game.data.drone_navigation=std::sync::Arc::new(crate::gamedata::DroneMap::of(Data { planets:HashMap::from([(game.planet_hash(),Vec::new().into())]),..Default::default() }));
    let mut drones=Vec::new();
    for i in 0..40 {
        let d=build(&mut game,"Drone1",[10.0+i as f32,4.0,10.0]);
        add_to_fleet(&mut game,d,false);
        set_target(&mut game,d,[1000.0,2.0,10.0]);
        drones.push(d);
    }
    game.ext.logistics.enabled=true;
    update(&mut game);
    assert_eq!(game.ext.logistics.drones.values().filter(|d| d.route_goal.is_some()).count(),ROUTES_PER_FRAME);
    let waiting=drones[ROUTES_PER_FRAME];
    assert_eq!(game.ext.logistics.drones[&waiting].position,[10.0+ROUTES_PER_FRAME as f32,4.0,10.0],"no movement without a checked route");
    // Busy first slots must not starve later drones when their own targets keep changing.
    for &d in &drones[..ROUTES_PER_FRAME] { set_target(&mut game,d,[1500.0,2.0,10.0]); }
    update(&mut game);
    assert!(drones[ROUTES_PER_FRAME..2*ROUTES_PER_FRAME].iter().all(|d| game.ext.logistics.drones[d].route_goal.is_some()));
    assert_eq!(game.ext.logistics.drones[&drones[0]].route_goal,Some([1000.0,4.0,10.0]));
    for _ in 0..3 { update(&mut game); }
    assert!(game.ext.logistics.drones.values().all(|d| d.route_goal==Some(d.target)));
}

/// The rules drones.nearestFirst and drones.avoidObstacles are off by default, and then the
/// game's order and the game's straight flight are what run.
#[test]
fn with_the_drone_rules_off_dispatch_and_flight_are_the_games() {
    let Some(mut game) = game() else { return };
    assert!(!game.rules.drones.nearest_first && !game.rules.drones.avoid_obstacles);
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    // The first registered supply serves the demand, however far it is.
    let far = build(&mut game, "Container1", [1000.0, 2.0, 20.0]);
    let near = build(&mut game, "Container1", [30.0, 2.0, 20.0]);
    let demand = build(&mut game, "Container1", [35.0, 2.0, 20.0]);
    let [far, near, demand] = [far, near, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    for supply in [far, near] {
        add(&mut game, "Iron", supply);
        game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
        set_dirty(&mut game, supply);
    }
    game.inventories.get_mut(&demand).unwrap().size = 1;
    game.inventories.get_mut(&demand).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, demand);
    set_logistic_tasks(&mut game);
    let supplies: Vec<_> = game.ext.logistics.all_tasks.iter().map(|(_, u)| game.ext.logistics.tasks[u].supply).collect();
    assert_eq!(supplies, [Some(far)], "the game takes the supplies in their order");
    // A drone takes the first free task, not the nearest.
    let first = game.ext.logistics.all_tasks.iter().map(|(_, u)| *u).next().unwrap();
    let drone = build(&mut game, "Drone1", [10.0, 4.0, 10.0]);
    add_to_fleet(&mut game, drone, true);
    assert_eq!(drone_task(&game, drone), Some(first));
    // A collision map is not used while the rule is off: the drone flies straight.
    use crate::geometry::navigation::{Bounds, Data};
    let planet = game.planet_hash();
    game.data.drone_navigation = std::sync::Arc::new(crate::gamedata::DroneMap::of(Data { format: 1, build_id: "t".into(), planets: HashMap::from([(planet, vec![Bounds { min: [0.0; 3], max: [2000.0, 500.0, 2000.0] }].into())]), buildings: HashMap::new() }));
    let start = game.ext.logistics.last_tick;
    for t in 1..=10 {
        frame(&mut game, start + t);
        game.net.out.clear();
    }
    assert!(game.ext.logistics.navigation.is_empty() && game.ext.logistics.drones[&drone].route_goal.is_none());
}

/// Without drones.keepCargo the cargo of a delivery that fails is destroyed, as the game's
/// EndAssignement does (the same flight as a_drone_preserves_cargo_if_destination_fills_during_flight).
#[test]
fn a_drone_loses_cargo_if_destination_fills_during_flight_as_in_the_game() {
    let Some(mut game) = game() else { return };
    build(&mut game, "EnergyGenerator6", [40.0, 2.0, 0.0]);
    let station = build(&mut game, "DroneStation1", [10.0, 2.0, 10.0]);
    let supply = build(&mut game, "Container1", [25.0, 2.0, 10.0]);
    let demand = build(&mut game, "Container1", [25.0, 2.0, 30.0]);
    let [station, supply, demand] = [station, supply, demand].map(|w| game.wos[&w].linked_inventory);
    let iron = game.data.group("Iron").unwrap();
    let cargo = add(&mut game, "Iron", supply);
    game.inventories.get_mut(&supply).unwrap().logistic.supply = vec![iron];
    game.inventories.get_mut(&demand).unwrap().logistic.demand = vec![iron];
    set_dirty(&mut game, supply);
    set_dirty(&mut game, demand);
    let drone = add(&mut game, "Drone1", station);
    let start = game.ext.logistics.last_tick;
    let mut filled = false;
    for t in 1..=40 * 30 {
        frame(&mut game, start + t);
        if !filled && game.ext.logistics.drones.get(&drone).is_some_and(|d| items(&game, d.inventory).contains(&cargo)) {
            filled = true;
            let capacity = game.inventories[&demand].size;
            for _ in 0..capacity {
                add(&mut game, "ice", demand);
            }
        }
    }
    assert!(filled, "control: the drone picked up the iron before the destination filled");
    assert!(!game.wos.contains_key(&cargo), "the game destroys what the drone still carries");
    assert!(items(&game, station).contains(&drone), "the drone still returns home");
}
