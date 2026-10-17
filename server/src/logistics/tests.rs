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
