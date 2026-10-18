//! What clients can do to the world through the RPCs the game's host accepts, and what the
//! server refuses: game-logic review findings, each as a test.

use super::tests::{join, save};
use super::*;

/// An RPC as a client sends it: to `object`, or with 0 to the scene handler of its class.
fn rpc(g: &mut Game, client: u64, object: u64, name: &str, args: Vec<Value>) {
    if object == 0 {
        return crate::testkit::send_scene(g, client, name, &args);
    }
    crate::testkit::send_on(g, client, object, name, &args);
}

/// Tick cost of the logistics task search and of destroys in a world with many stored items.
/// The review's setup: PCDS_REVIEW_CHESTS=1000 PCDS_REVIEW_GROWING=1 (32k stored items, 2000
/// vegetables growing), 30 demand containers, 20 s of game time.
#[test]
#[ignore]
fn perf_logistics_and_destroy() {
    let Some(mut g) = crate::machines::tests::game() else { return };
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (chests, loose, demands) = (env("PCDS_REVIEW_CHESTS", 1000), env("PCDS_REVIEW_LOOSE", 2000), env("PCDS_REVIEW_DEMANDS", 30));
    g.sim_time = Some(1.0);
    g.tick();
    crate::machines::tests::build(&mut g, "DroneStation1", [600.0, 5.0, 600.0]);
    let iron = g.data.group("Iron").unwrap();
    for _ in 0..chests {
        g.create_inventory(30, 0, 0, &[iron; 30], &[]);
    }
    let veg = g.data.group("Vegetable0Growable").unwrap();
    for i in 0..loose {
        let id = g.create_world_object(veg, 0, true);
        g.drop_on_floor(id, [500.0 + (i % 50) as f32, 5.0, 500.0 + (i / 50) as f32], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 0);
        if std::env::var_os("PCDS_REVIEW_GROWING").is_some() {
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
    let started = std::time::Instant::now();
    for _ in 0..20 * 200 {
        t += 0.005;
        g.sim_time = Some(t);
        let t0 = std::time::Instant::now();
        g.tick();
        worst = worst.max(t0.elapsed().as_secs_f64());
        g.net.out.clear();
    }
    println!("20 s of game: {:.2} s wall, worst tick {:.1} ms", started.elapsed().as_secs_f64(), worst * 1000.0);
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
    let text = crate::handoff::snapshot(&g).unwrap();
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
    let text = crate::handoff::snapshot(&g).unwrap();
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let mut h = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap();
    let e = h.create_world_object(iron, 0, true);
    assert_eq!(h.wos.keys().copied().filter(|id| [c, d, e].contains(id)).collect::<Vec<_>>(), vec![e, d, c]);
}

/// H3: a snapshot of the previous format (world objects as a map, the sets as lists) restores.
#[test]
fn a_format_1_snapshot_restores() {
    let Some(mut g) = test_game(save()) else { return };
    join(&mut g, 1, "Ann");
    g.disconnect(1);
    let text = crate::handoff::snapshot(&g).unwrap();
    let (_, state) = text.split_once('\n').unwrap();
    let mut state: Value = serde_json::from_str(state).unwrap();
    let pairs = |v: &Value| v["slots"].as_array().unwrap().iter().filter(|s| !s.is_null()).map(|s| (s[0].clone(), s[1].clone())).collect::<Vec<_>>();
    let wos: serde_json::Map<String, Value> = pairs(&state["wos"]).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    state["wos"] = Value::Object(wos);
    for set in ["constructed", "pickables"] {
        state[set] = Value::Array(pairs(&state[set]).into_iter().map(|(k, _)| k).collect());
    }
    let old = format!("{{\"format\":1,\"game_version\":\"{}\"}}\n{state}", g.net_data.game_version);
    let (net_data, data, spec, scenes) = test_tables().unwrap();
    let h = crate::handoff::restore(net_data, data, spec, &old, scenes).unwrap();
    crate::handoff::verify(&h, text.as_bytes()).unwrap();
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
