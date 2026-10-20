//! What clients can do to the world through the RPCs the game's host accepts, and what the
//! server refuses: game-logic review findings, each as a test.

use super::tests::{join, save};
use super::*;

/// An RPC as a client sends it: to `object`, or with 0 to the scene handler of its class. It
/// runs now: what the tick's budget queued runs as the next tick would.
fn rpc(g: &mut Game, client: u64, object: u64, name: &str, args: Vec<Value>) {
    match object {
        0 => crate::testkit::send_scene(g, client, name, &args),
        _ => crate::testkit::send_on(g, client, object, name, &args),
    }
    crate::handlers::run_pending(g, |g, rpc| g.rpc_cost(rpc), |g, client, rpc| g.run_rpc(client, rpc));
}

/// Tick cost of the logistics task search and of destroys in a world with many stored items.
/// The review's setup: 1000 chests of 30 items (32k stored items), 2000 loose vegetables (set
/// GROWING for them to grow, in a grower), 30 demand containers, 20 s of game time.
#[test]
#[ignore]
fn perf_logistics_and_destroy() {
    let Some(mut g) = crate::machines::tests::game() else { return };
    const GROWING: bool = false;
    let (chests, loose, demands) = (1000, 2000, 30);
    g.sim_time = Some(1.0);
    g.tick();
    let env = |name: &str, default: usize| crate::env(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    for i in 0..env("PCDS_REVIEW_STATIONS", 1) {
        crate::machines::tests::build(&mut g, "DroneStation1", [600.0 + 12.0 * i as f32, 5.0, 600.0]);
    }
    let iron = g.data.group("Iron").unwrap();
    for _ in 0..chests {
        g.create_inventory(30, 0, 0, &[iron; 30], &[]);
    }
    let veg = g.data.group("Vegetable0Growable").unwrap();
    for i in 0..loose {
        let id = g.create_world_object(veg, 0, true);
        g.drop_on_floor(id, [500.0 + (i % 50) as f32, 5.0, 500.0 + (i / 50) as f32], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 0);
        if GROWING {
            // Growing in a grower's inventory: placed, held, not grown.
            let inv = g.create_inventory(1, 0, 0, &[], &[]);
            g.inventory_add(inv, id, false);
            g.wos.get_mut(&id).unwrap().growth = 50.0;
        }
    }
    let hash = g.data.groups[veg].hash;
    for i in 0..demands {
        let c = crate::machines::tests::build(&mut g, "Container2", [700.0 + i as f32 * 3.0, 5.0, 700.0]);
        let inv = g.wos[&c].linked_inventory;
        g.inventories_rpc(1, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &Args(vec![json!(inv), json!([hash]), json!([]), json!(0)]));
    }
    let items: usize = g.inventories.values().map(|i| i.items.len()).sum();
    println!("world: wos {} inventories {} stored items {} pickables {}", g.wos.len(), g.inventories.len(), items, g.pickables.len());
    let mut t = 2.0;
    let mut worst = 0.0f64;
    let seconds = env("PCDS_REVIEW_SECONDS", 20);
    let started = std::time::Instant::now();
    let mut minute_worst = 0.0f64;
    for i in 0..seconds * 200 {
        t += 0.005;
        g.sim_time = Some(t);
        let t0 = std::time::Instant::now();
        g.tick();
        let took = t0.elapsed().as_secs_f64();
        worst = worst.max(took);
        minute_worst = minute_worst.max(took);
        g.net.out.clear();
        if i % (60 * 200) == 60 * 200 - 1 {
            println!("game minute {}: worst tick {:.1} ms, wos {} inventories {}", (i + 1) / 12000, minute_worst * 1000.0, g.wos.len(), g.inventories.len());
            minute_worst = 0.0;
        }
    }
    println!("{seconds} s of game: {:.2} s wall, worst tick {:.1} ms", started.elapsed().as_secs_f64(), worst * 1000.0);
    let some: Vec<i32> = g.inventories.values().flat_map(|i| i.items.iter().copied()).take(1000).collect();
    let t0 = std::time::Instant::now();
    g.destroy_world_objects(&some, false);
    println!("destroying 1000 stored items: {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
}

/// H1: GroupNetworkBase.DeconstructServerRpc does nothing on what has no ActionDeconstructible;
/// a building that has one still comes apart.
#[test]
fn only_deconstructible_objects_come_apart() {
    let Some(mut g) = crate::planets::tests::game() else { return };
    join(&mut g, 1, "Ann");
    g.sim_time = Some(1.0);
    g.tick();
    let iron = g.data.group("Iron").unwrap();
    for (group, comes_apart) in [("SpaceMultiplierOxygen", false), ("WreckSafe", false), ("GoldenEffigie1", false), ("Container2", true)] {
        let id = crate::machines::tests::build(&mut g, group, [-500.0, -500.0, -500.0]);
        let inv = g.wos[&id].linked_inventory;
        if inv != 0 && !comes_apart {
            g.add_new_item(iron, inv, 0);
        }
        let net = g.wos[&id].net.unwrap();
        g.net.out.clear();
        rpc(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", vec![json!(0.0)]);
        assert_eq!(g.net.out.is_empty(), !comes_apart, "{group}: the game answers nothing");
        let t = g.sim_time.unwrap() + 0.5;
        g.sim_time = Some(t);
        g.tick();
        assert_eq!(!g.wos.contains_key(&id), comes_apart, "{group}");
        if inv != 0 {
            assert_eq!(!g.inventories.contains_key(&inv), comes_apart, "{group}'s inventory");
        }
    }
}

/// L1: a transfer from an inventory to itself changes nothing (the game loses the item).
#[test]
fn a_transfer_to_the_same_inventory_keeps_the_item() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    let inv = g.clients[&1].ready.as_ref().unwrap().inventory;
    let items = g.inventories[&inv].items.clone();
    for name in ["InventoriesHandler.TransferItemServerRpc", "InventoriesHandler.TransferAllItemsServerRpc"] {
        let args = if name.contains("All") { vec![json!(inv), json!(inv)] } else { vec![json!(inv), json!(inv), json!(items[0])] };
        rpc(&mut g, 1, 0, name, args);
        assert_eq!(g.inventories[&inv].items, items, "{name}");
    }
}

/// L2: adding a new item to an inventory that refuses it leaves no world object behind.
#[test]
fn a_refused_new_item_is_not_kept() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    let inv = g.create_inventory(0, 0, 0, &[], &[]);
    let (before, saved) = (g.wos.len(), g.snapshot().objects.len());
    let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
    for _ in 0..10 {
        rpc(&mut g, 1, 0, "InventoriesHandler.AddNewItemToInventoryServerRpc", vec![json!(iron), json!(inv)]);
    }
    assert_eq!((g.wos.len(), g.snapshot().objects.len()), (before, saved));
    assert_eq!(g.group_count(false, g.data.group("Iron").unwrap()), 0, "the group count is back");
}

/// The holder index against the inventories themselves: what is wrong, if anything.
fn holders_mismatch(g: &Game) -> Option<String> {
    let mut want: HashMap<i32, (Vec<i32>, u32)> = HashMap::new();
    for inv in g.inventories.values() {
        for wo in &inv.items {
            let e = want.entry(*wo).or_default();
            e.0.push(inv.id);
            e.1 += 1;
        }
    }
    if want.len() != g.holders.len() {
        return Some(format!("{} held world objects, the index has {}", want.len(), g.holders.len()));
    }
    for (wo, (invs, n)) in want {
        match g.holders.get(&wo) {
            Some(&(inv, count)) if count == n && (n > 1 || inv == invs[0]) => {}
            other => return Some(format!("wo {wo} in {invs:?}, the index says {other:?}")),
        }
    }
    None
}

/// H2: the index follows every way items come and go, a save's double holders included.
#[test]
fn the_holder_index_follows_the_inventories() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    let inv = g.clients[&1].ready.as_ref().unwrap().inventory;
    assert_eq!(holders_mismatch(&g), None);
    let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
    let chest = g.create_inventory(10, 0, 0, &[], &[]);
    for _ in 0..4 {
        rpc(&mut g, 1, 0, "InventoriesHandler.AddNewItemToInventoryServerRpc", vec![json!(iron), json!(chest)]);
    }
    let items = g.inventories[&chest].items.clone();
    assert_eq!(g.holder(items[0]), Some(chest));
    rpc(&mut g, 1, 0, "InventoriesHandler.TransferItemServerRpc", vec![json!(chest), json!(inv), json!(items[0])]);
    assert_eq!(g.holder(items[0]), Some(inv));
    rpc(&mut g, 1, 0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", vec![json!(items[1]), json!(chest), json!(false)]);
    assert!(!g.is_held(items[1]));
    rpc(&mut g, 1, 0, "InventoriesHandler.RemoveItemsFromInventoryServerRpc", vec![json!([iron]), json!(chest), json!(true)]);
    rpc(&mut g, 1, 0, "InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc", vec![json!([items[2]]), json!(chest), json!(false)]);
    rpc(&mut g, 1, 0, "InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", vec![json!(inv)]);
    assert_eq!(holders_mismatch(&g), None);
    // A saved world object in two inventories: held twice, and still held once one lets go.
    let wo = items[2];
    g.inventory_add(chest, wo, true);
    g.inventories.get_mut(&inv).unwrap().items.push(wo);
    g.rebuild_holders();
    assert_eq!(g.holders[&wo].1, 2);
    assert_eq!(g.holder(wo), g.inventories.values().find(|i| i.items.contains(&wo)).map(|i| i.id), "the first, as the game finds it");
    g.inventory_remove(chest, wo);
    assert_eq!(g.holder(wo), Some(inv));
    g.destroy_inventory(chest, true);
    g.destroy_world_object(wo, true);
    assert_eq!(holders_mismatch(&g), None);
    // A handoff restores it.
    let text = crate::handoff::snapshot(&mut g).unwrap();
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let restored = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap();
    assert_eq!(restored.holders, g.holders);
}

/// L3: an item added to an inventory leaves the one it was in; it is never in two.
#[test]
fn an_added_existing_item_leaves_its_old_inventory() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    let iron = g.data.group("Iron").unwrap();
    let a = g.create_inventory(10, 0, 0, &[iron], &[]);
    let b = g.create_inventory(10, 0, 0, &[], &[]);
    let wo = g.inventories[&a].items[0];
    rpc(&mut g, 1, 0, "InventoriesHandler.AddExistingItemToInventoryServerRpc", vec![json!(wo), json!(b), json!(true)]);
    assert_eq!((g.inventories[&a].items.clone(), g.inventories[&b].items.clone()), (vec![], vec![wo]));
    assert_eq!(holders_mismatch(&g), None);
    let saved = g.snapshot().inventories;
    assert_eq!(saved.iter().filter(|i| i["woIds"].as_str().is_some_and(|s| s.split(',').any(|x| x == wo.to_string()))).count(), 1);
}

/// H3: world objects keep the order of the game's Dictionary: a destroyed one's place goes to
/// the next one created, and the save lists them so.
#[test]
fn world_objects_keep_the_dictionary_order() {
    let Some(mut g) = test_game(save()) else { return };
    let iron = g.data.group("Iron").unwrap();
    let [a, b, c] = [0; 3].map(|_| g.create_world_object(iron, 0, true));
    g.destroy_world_object(b, true);
    let d = g.create_world_object(iron, 0, true);
    let order: Vec<i32> = g.wos.keys().copied().filter(|id| [a, c, d].contains(id)).collect();
    assert_eq!(order, vec![a, d, c]);
    let saved: Vec<i64> = g.snapshot().objects.iter().filter_map(|o| o["id"].as_i64()).filter(|id| [a, c, d].contains(&(*id as i32))).collect();
    assert_eq!(saved, vec![a as i64, d as i64, c as i64]);
    // A handoff keeps the free places: the next one created goes where the next would have.
    g.destroy_world_object(a, true);
    let text = crate::handoff::snapshot(&mut g).unwrap();
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let mut h = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap();
    let e = h.create_world_object(iron, 0, true);
    assert_eq!(h.wos.keys().copied().filter(|id| [c, d, e].contains(id)).collect::<Vec<_>>(), vec![e, d, c]);
}

/// The save lists the inventories in the order of the game's Dictionary, as it does the world
/// objects: a destroyed one's place goes to the next created, the most recently freed first (the
/// sweep's truck, whose two inventories come after two destroyed ones, lists its second first).
#[test]
fn inventories_keep_the_dictionary_order() {
    let Some(mut g) = test_game(save()) else { return };
    let [a, b, c] = [0; 3].map(|_| g.create_inventory(5, 0, 0, &[], &[]));
    g.destroy_inventory(a, true);
    g.destroy_inventory(c, true);
    let [d, e] = [0; 2].map(|_| g.create_inventory(5, 0, 0, &[], &[]));
    let ours = |g: &mut Game| g.snapshot().inventories.iter().filter_map(|i| i["id"].as_i64()).map(|i| i as i32).filter(|i| [b, d, e].contains(i)).collect::<Vec<_>>();
    // c's place went first to d, then a's to e: e comes before b, d after.
    assert_eq!(ours(&mut g), vec![e, b, d]);
    let text = crate::handoff::snapshot(&mut g).unwrap();
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let mut h = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap();
    assert_eq!(ours(&mut h), vec![e, b, d]);
}

/// M2: an RPC runs only on an object with a behaviour of its class (or a subclass), as Netcode
/// routes it: a departure sequence started on anything but a departure platform does nothing
/// (it used to force a full save each time).
#[test]
fn rpcs_run_only_on_objects_of_their_class() {
    let Some(mut g) = crate::planets::tests::game() else { return };
    join(&mut g, 1, "Ann");
    let chest = crate::machines::tests::build(&mut g, "Container1", [600.0, 5.0, 600.0]);
    let platform = crate::machines::tests::build(&mut g, "DeparturePlatform", [650.0, 5.0, 600.0]);
    let start = |g: &mut Game, net: u64| {
        g.save_requested = false;
        let (hash, body) = g.net.spec.encode("MachineDeparturePlatform.StartDepartureSequenceServerRpc", &[json!(1), json!(0), json!(0)]);
        let mut w = Writer::new();
        w.packed_u64(1).packed_u64(net).packed_u16(0).packed_u32(hash).bytes(&body);
        g.incoming_rpc(1, &w.0);
        crate::handlers::run_pending(g, |g, rpc| g.rpc_cost(rpc), |g, client, rpc| g.run_rpc(client, rpc));
        g.save_requested
    };
    let objects: Vec<u64> = g.net.objects.keys().copied().filter(|o| !g.net.carries(*o, "MachineDeparturePlatform")).collect();
    for net in objects {
        assert!(!start(&mut g, net), "object {net}");
    }
    let platform_net = g.wos[&platform].net.unwrap();
    assert!(start(&mut g, platform_net), "a departure platform starts");
    // GroupNetworkBase's RPCs run on its subclasses' behaviours.
    let chest_net = g.wos[&chest].net.unwrap();
    assert!(g.net.carries(chest_net, "GroupNetworkBase") && !g.net.carries(chest_net, "MachineDeparturePlatform"));
}

/// M3: a player's name keeps none of the save's delimiters nor a quote, so the game splits the
/// save (on '@', then '|') and finds the name ("name":"[^"]*?",) as written.
#[test]
fn player_names_cannot_break_the_save() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "a\"|b@c");
    g.disconnect(1);
    let text = g.snapshot().to_text();
    let sections: Vec<&str> = text.split('@').collect();
    assert_eq!(sections.len(), 11, "ten sections, each ended by '@'");
    let players: Vec<&str> = sections[2].split('|').collect();
    assert_eq!(players.len(), 1);
    let at = players[0].find("\"name\":\"").unwrap() + 8;
    let name = &players[0][at..at + players[0][at..].find('"').unwrap()];
    assert_eq!(name, "a' b c");
    assert_eq!(SaveFile::from_text(&text).players[0]["name"], "a' b c");
}

/// H4: one player cannot empty, take from, resize, lock or destroy another's backpack or
/// equipment, in the game or saved; their own still works, and so do the server's own changes
/// (a death drop).
#[test]
fn other_players_inventories_are_theirs() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    join(&mut g, 2, "Bob");
    let mine = g.clients[&1].ready.as_ref().unwrap().inventory;
    let (bob_inv, bob_eq) = g.clients[&2].ready.as_ref().map(|s| (s.inventory, s.equipment)).unwrap();
    let before = g.inventories[&bob_inv].clone();
    assert!(!before.items.is_empty(), "Bob has the starter kit");
    let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
    let item = before.items[0];
    let attacks: Vec<(&str, Vec<Value>)> = vec![
        ("InventoriesHandler.TransferAllItemsServerRpc", vec![json!(bob_inv), json!(mine)]),
        ("InventoriesHandler.TransferItemServerRpc", vec![json!(bob_inv), json!(mine), json!(item)]),
        ("InventoriesHandler.AddExistingItemToInventoryServerRpc", vec![json!(item), json!(mine), json!(true)]),
        ("InventoriesHandler.RemoveAndDestroyAllItemsFromInventoryServerRpc", vec![json!(bob_inv)]),
        ("InventoriesHandler.RemoveWorldObjectsFromInventoryServerRpc", vec![json!([item]), json!(bob_inv), json!(true)]),
        ("InventoriesHandler.SetInventorySizeServerRpc", vec![json!(bob_inv), json!(-1000), json!([0.0, 0.0, 0.0])]),
        ("InventoriesHandler.LockInventoryContentServerRpc", vec![json!(bob_inv), json!(true), json!(1.0e6)]),
        ("InventoriesHandler.AddNewItemToInventoryServerRpc", vec![json!(iron), json!(bob_eq)]),
        ("WorldObjectsHandler.DestroyWorldObjectServerRpc", vec![json!(item), json!(true)]),
        // Decoding uses the item up (a chip, or any item: the game does not look).
        ("UnlockedGroupsHandler.DecodeBlueprintServerRpc", vec![json!(item)]),
        ("WorldObjectsHandler.DropOnFloorServerRpc", vec![json!(item), json!([1.0, 2.0, 3.0]), json!(0.0), json!(false), json!(false), json!(0), json!(0.0)]),
        // An object linked to Bob's backpack, then destroyed: it would take the backpack along.
        ("WorldObjectsHandler.GenerateRandomAppearanceObjectServerRpc", vec![json!(100.0), json!(iron), json!(205_555_555), json!(bob_inv), json!([1.0, 2.0, 3.0]), json!([0.0, 0.0, 0.0, 1.0])]),
        ("WorldObjectsHandler.DestroyWorldObjectServerRpc", vec![json!(205_555_555), json!(true)]),
    ];
    for (name, args) in &attacks {
        rpc(&mut g, 1, 0, name, args.clone());
        let now = &g.inventories[&bob_inv];
        assert_eq!((&now.items, now.size), (&before.items, before.size), "{name}");
        assert!(g.wos.get(&item).is_some_and(|w| w.lock_time == crate::clock::GameTime::ZERO && !w.is_placed()), "{name}");
        assert!(g.inventories[&bob_eq].items.is_empty(), "{name}");
    }
    // Saved, not in the game: still Bob's.
    g.disconnect(2);
    rpc(&mut g, 1, 0, "InventoriesHandler.TransferAllItemsServerRpc", vec![json!(bob_inv), json!(mine)]);
    assert_eq!(g.inventories[&bob_inv].items, before.items);
    // Ann's own inventory is hers.
    rpc(&mut g, 1, 0, "InventoriesHandler.AddNewItemToInventoryServerRpc", vec![json!(iron), json!(mine)]);
    assert_eq!(g.inventories[&mine].items.len(), before.items.len() + 1);
    // The server's own change: Bob comes back and dies, and drops his things.
    join(&mut g, 2, "Bob");
    assert_eq!(g.clients[&2].ready.as_ref().unwrap().inventory, bob_inv);
    g.rules.death.consequences = Some("DropAllItems".to_string());
    crate::status::handle_dying_consequences(&mut g, 2);
    assert!(g.inventories[&bob_inv].items.len() < before.items.len(), "a death drop takes from the backpack");
}

/// H4: an inventory id is never handed out while a player's (gone) inventory has it, and a
/// player whose saved backpack is gone gets a new empty one.
#[test]
fn player_inventory_ids_stay_theirs() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    join(&mut g, 2, "Bob");
    let bob_inv = g.clients[&2].ready.as_ref().unwrap().inventory;
    g.disconnect(2);
    g.destroy_inventory(bob_inv, true);
    assert!(g.inventories.contains_key(&bob_inv), "a player's inventory is never destroyed");
    // As an older server could leave it: gone.
    let gone = g.inventories.remove(&bob_inv).unwrap();
    gone.items.iter().for_each(|wo| g.release(*wo));
    for _ in 0..3 {
        assert_ne!(g.create_inventory(5, 0, 0, &[], &[]), bob_inv);
    }
    assert_ne!(g.create_inventory(5, bob_inv, 0, &[], &[]), bob_inv, "not by asking for it either");
    join(&mut g, 2, "Bob");
    let inv = g.clients[&2].ready.as_ref().unwrap().inventory;
    assert!(g.inventories.get(&inv).is_some_and(|i| i.items.is_empty() && i.size == START_INVENTORY_SIZE));
    assert_eq!(holders_mismatch(&g), None);
}

/// L4: the constructed-objects index goes on counting past 2^24 (a float stops there, and the
/// units would never be recounted again).
#[test]
fn the_constructed_index_never_stops() {
    let Some(mut g) = test_game(save()) else { return };
    g.units.constructed_index = 1 << 24;
    crate::units::constructed_changed(&mut g);
    assert_eq!(g.units.constructed_index, (1 << 24) + 1);
}

/// L5: ids and hashes a client makes up keep nothing in memory: watches of missing
/// inventories, logistics of missing inventories, unlocks of missing groups.
#[test]
fn made_up_ids_keep_nothing() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    let (watchers, entities) = (g.watchers.len(), serde_json::to_value(&g.ext.logistics).unwrap()["entities"].as_object().map(|o| o.len()));
    let unlocked = |g: &Game| match g.net.find_class("UnlockedGroupsHandler").and_then(|(o, _)| g.net.get(o, "UnlockedGroupsHandler", "_unlockedGroups")) {
        Some(crate::net::VarValue::List(items)) => items.len(),
        _ => 0,
    };
    let before = unlocked(&g);
    for i in 0..100 {
        rpc(&mut g, 1, 0, "InventoriesHandler.BeginInventoryWatchServerRpc", vec![json!(5_000_000 + i)]);
        rpc(&mut g, 1, 0, "InventoriesHandler.CheckInventoryWatchAndDirtyServerRpc", vec![json!(6_000_000 + i)]);
        rpc(&mut g, 1, 0, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", vec![json!(7_000_000 + i), json!([]), json!([]), json!(0)]);
        rpc(&mut g, 1, 0, "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc", vec![json!(123_456 + i), json!(false), json!(false)]);
    }
    assert_eq!(g.watchers.len(), watchers);
    assert_eq!(serde_json::to_value(&g.ext.logistics).unwrap()["entities"].as_object().map(|o| o.len()), entities);
    assert_eq!(unlocked(&g), before);
    // A real group still unlocks.
    let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
    rpc(&mut g, 1, 0, "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc", vec![json!(iron), json!(false), json!(false)]);
    assert_eq!(unlocked(&g), before + 1);
}

/// L7: a panic in the admin's or a chat hook's work is caught and logged, and the bank goes on
/// (it ended the bank, and every player's session, until the front started it from the save).
#[test]
fn a_panic_in_admin_work_does_not_end_the_bank() {
    let Some(mut g) = test_game(save()) else { return };
    assert_eq!(crate::bank::contained("a test request", || -> u16 { panic!("test panic") }), None);
    assert_eq!(crate::bank::contained("a test request", || { crate::chathooks::say(&mut g, "hello"); 200 }), Some(200));
}

/// What a world must keep whatever clients send: every item in one inventory at most, held items
/// exist, saved players' inventories exist, and the holder index matches.
fn invariants(g: &Game) -> Vec<String> {
    let mut bad = Vec::new();
    let mut holder: HashMap<i32, i32> = HashMap::new();
    for inv in g.inventories.values() {
        for wo in &inv.items {
            if let Some(prev) = holder.insert(*wo, inv.id).filter(|p| *p != inv.id) {
                bad.push(format!("wo {wo} in inventories {prev} and {}", inv.id));
            }
            if !g.wos.contains_key(wo) {
                bad.push(format!("inventory {} holds missing wo {wo}", inv.id));
            }
        }
    }
    let ready = g.clients.values().filter_map(|c| c.ready.as_ref()).flat_map(|s| [s.inventory, s.equipment]);
    let saved = g.saved_players.iter().flat_map(|p| ["inventoryId", "equipmentId"].map(|k| p[k].as_i64().unwrap_or(-1) as i32));
    for id in ready.chain(saved) {
        if !g.inventories.contains_key(&id) {
            bad.push(format!("player inventory {id} missing"));
        }
    }
    bad.extend(holders_mismatch(g));
    for w in g.wos.values() {
        if w.net.is_some_and(|n| !g.net.objects.contains_key(&n)) {
            bad.push(format!("world object {} ({}) has net object {:?}, which is gone", w.id, g.data.groups[w.group].id, w.net));
        }
    }
    for id in g.constructed.iter().chain(g.pickables.iter()) {
        if !g.wos.contains_key(id) {
            bad.push(format!("constructed or pickable {id} is not a world object"));
        }
    }
    // (An animal stands for its DNA sequence, which has no network object of its own.)
    for o in g.net.objects.values().filter(|o| !o.scene && !crate::animals::spawn::is_animal(g, o.id)) {
        if let Some(w) = o.wo {
            match g.wos.get(&w) {
                // A plant of a grower whose item was destroyed keeps its network object on purpose (the game's
                // own: objects.rs destroy_world_object, the parity sweep): it is the machine's child and goes
                // with the machine.
                None if g.ext.machines.grower_of(w).is_some_and(|m| g.wos.contains_key(&m)) => {}
                None => bad.push(format!("net object {} stands for world object {w}, which is gone", o.id)),
                Some(wo) if wo.net != Some(o.id) => bad.push(format!("net object {} stands for world object {w} ({}), whose net is {:?}", o.id, g.data.groups[wo.group].id, wo.net)),
                _ => {}
            }
        }
    }
    bad
}

/// Hostile RPCs on the bench's real world (buildings, machines, drones), two players, ticks in
/// between: no panic, and the invariants hold, also in the world loaded from its save.
/// PCDS_BENCH_SAVE=world.json [PCDS_FUZZ_ROUNDS=20000] [PCDS_FUZZ_SEED=7].
#[test]
#[ignore]
fn hostile_rpcs_on_a_real_world_keep_it_whole() {
    let Some(path) = std::env::var("PCDS_BENCH_SAVE").ok() else { return };
    let Some((net_data, data, spec, scenes)) = test_tables() else { return };
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (rounds, mut seed) = (env("PCDS_FUZZ_ROUNDS", 20000), env("PCDS_FUZZ_SEED", 7));
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed >> 33
    };
    let mut g = Game::new(net_data, data, spec, SaveFile::load(&path).unwrap(), scenes);
    g.sim_time = Some(10.0);
    g.tick();
    join(&mut g, 1, "Ann");
    join(&mut g, 2, "Bob");
    let table: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/rpcs.json", crate::test_data().unwrap())).unwrap()).unwrap();
    let mut names: Vec<String> = table.as_object().unwrap().values().filter(|r| r["to"] == "server").filter_map(|r| r["name"].as_str().map(String::from)).filter(|n| n != "PlayerMainController.PlayerReadyServerRpc").collect();
    names.sort();
    let (mut t, mut sent) = (10.0, 0);
    // PCDS_FUZZ_LOG=file: every RPC sent, to find what led to a failure.
    let mut log = std::env::var("PCDS_FUZZ_LOG").ok().and_then(|p| std::fs::File::create(p).ok());
    for round in 0..rounds {
        let name = names[(next() % names.len() as u64) as usize].clone();
        let pools: [Vec<i64>; 4] = [
            g.wos.keys().map(|k| *k as i64).collect(),
            g.inventories.keys().map(|k| *k as i64).collect(),
            g.data.groups.iter().map(|gr| gr.hash as i64).collect(),
            g.clients.values().filter_map(|c| c.ready.as_ref()).flat_map(|s| [s.inventory as i64, s.equipment as i64]).collect(),
        ];
        let pick = |next: &mut dyn FnMut() -> u64| -> Value {
            let pool = &pools[(next() % 4) as usize];
            let any = |n: u64| pool.get((n % pool.len().max(1) as u64) as usize).copied().unwrap_or(0);
            match next() % 12 {
                0 => json!(-1),
                1 => json!(0),
                2 => json!(i32::MAX),
                3 => json!(-1000),
                4 => json!(true),
                5 => json!((0..next() % 4).map(|_| any(next())).collect::<Vec<_>>()),
                6 => json!([any(next()) as f32, 3.0, 2.0, 1.0]),
                _ => json!(any(next())),
            }
        };
        let n = 1 + next() % 7;
        let args: Vec<Value> = (0..n).map(|_| pick(&mut next)).collect();
        let client = 1 + next() % 2;
        let class = name.split('.').next().unwrap();
        let carriers: Vec<u64> = g.net.objects.keys().copied().filter(|o| g.net.carries(*o, class)).collect();
        let Some(&object) = carriers.get((next() % carriers.len().max(1) as u64) as usize) else { continue };
        let behaviour = g.net.behaviour_index(object, class).unwrap_or(0);
        let (hash, body) = g.net.spec.encode(&name, &args);
        let mut w = Writer::new();
        w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(hash).bytes(&body);
        let line = format!("{round} c{client} {name} {}", Value::Array(args));
        if let Some(log) = log.as_mut() {
            use std::io::Write;
            let _ = writeln!(log, "{line}");
        }
        sent += 1;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            g.incoming_rpc(client, &w.0);
            if round % 7 == 0 {
                t += 0.25;
                g.sim_time = Some(t);
                g.tick();
            }
        }));
        assert!(r.is_ok(), "panicked at {line}");
        g.net.out.clear();
        g.kicks.clear();
        let bad = invariants(&g);
        assert!(bad.is_empty(), "after {line}: {:?}", &bad[..bad.len().min(5)]);
    }
    println!("{sent} RPCs sent; world objects {}, inventories {}", g.wos.len(), g.inventories.len());
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let h = Game::new(net_data, data, spec, SaveFile::from_text(&g.snapshot().to_text()), scenes);
    assert_eq!(invariants(&h), Vec::<String>::new(), "the world loaded from its save");
}
