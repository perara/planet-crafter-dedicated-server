//! What clients can do to the world through the RPCs the game's host accepts, and what the
//! server refuses: game-logic review findings, each as a test.

use super::tests::{join, save};
use super::*;

/// An RPC as a client sends it.
fn rpc(g: &mut Game, client: u64, object: u64, name: &str, args: Vec<Value>) {
    let (hash, body) = g.net.spec.encode(name, &args);
    let mut w = Writer::new();
    w.packed_u64(client).packed_u64(object).packed_u16(0).packed_u32(hash).bytes(&body);
    g.incoming_rpc(client, &w.0);
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
