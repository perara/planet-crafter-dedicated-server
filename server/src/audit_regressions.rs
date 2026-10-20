use crate::testkit::*;
use serde_json::json;

#[test]
fn audit_deconstruction_preserves_items_added_during_animation() {
    let (mut g, _) = world().expect("real game tables required");
    let (chest, net) = place(&mut g, "Container1", [570.0, 3.0, 500.0]);
    let inv = g.wos[&chest].linked_inventory;
    send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
    assert!(!sent(&g, "GroupNetworkBase.StartDeconstructClientRpc").is_empty());
    let iron = g.data.group("Iron").unwrap();
    let hash = g.data.groups[iron].hash;
    send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(hash), json!(inv)]);
    let cargo = g.inventories[&inv].items[0];
    g.time += 3.0;
    crate::scene::tick(&mut g);
    assert!(g.wos.contains_key(&cargo), "cargo added during deconstruction vanished; chest still exists: {}", g.wos.contains_key(&chest));
}

#[test]
fn audit_new_rocket_is_boosted_without_reinserting_optimizer_fuse() {
    let (mut g, _) = world().expect("real game tables required");
    let (optimizer, _) = place(&mut g, "Optimizer2", [570.0, 3.0, 500.0]);
    let inv = g.wos[&optimizer].linked_inventory;
    let fuse = g.data.group("FuseTradeRocketsSpeed1").unwrap();
    let (ok, item) = g.add_new_item(fuse, inv, 0); assert!(ok);
    let (_, net) = place(&mut g, "TradePlatform1", [580.0, 3.0, 500.0]);
    g.net.out.clear();
    send_on(&mut g, 1, net, "ConstructibleProxy.RetrieveRocketBackAndForthSpeedMultiplierServerRpc", &[]);
    let before = one(&g, "ConstructibleProxy.SetRocketBackAndForthSpeedMultiplierClientRpc")[0].as_f64().unwrap();
    send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(item), json!(inv), json!(true)]);
    assert!(g.add_new_item(fuse, inv, 0).0);
    g.net.out.clear();
    send_on(&mut g, 1, net, "ConstructibleProxy.RetrieveRocketBackAndForthSpeedMultiplierServerRpc", &[]);
    let after = one(&g, "ConstructibleProxy.SetRocketBackAndForthSpeedMultiplierClientRpc")[0].as_f64().unwrap();
    assert!(after > 1.0, "control: reinserting the fuse must boost the rocket");
    assert!(before > 1.0, "newly built rocket multiplier was {before}; after reinserting the same fuse it is {after}");
}

#[test]
fn audit_joiner_receives_existing_optimizer_growth_boost() {
    let (mut g, _) = world().expect("real game tables required");
    let (optimizer, _) = place(&mut g, "Optimizer2", [570.0, 3.0, 500.0]);
    let (_, grower) = place(&mut g, "Vegetube1", [580.0, 3.0, 500.0]);
    let inv = g.wos[&optimizer].linked_inventory;
    let fuse = g.data.group("FuseGrowth1").unwrap();
    g.net.out.clear();
    assert!(g.add_new_item(fuse, inv, 0).0);
    let boosted = sent(&g, "ConstructibleProxy.SetGrowerSpeedMultiplierClientRpc");
    assert!(boosted.iter().any(|m| m.object == grower && m.args[0].as_f64().unwrap() > 0.0), "control: optimizer applied its boost");
    g.net.out.clear();
    join(&mut g, 2, "Bob");
    let sync = sent(&g, "ConstructibleProxy.SetGrowerSpeedMultiplierClientRpc");
    assert!(sync.iter().any(|m| m.object == grower && m.to == crate::net::Target::Only(2)), "new client got {} growth-speed messages despite an active growth fuse", sync.len());
}

#[test]
fn audit_enabling_full_destructor_clears_its_inventory() {
    let (mut g, _) = world().expect("real game tables required");
    let (destructor, net) = place(&mut g, "Destructor1", [570.0, 3.0, 500.0]);
    let inv = g.wos[&destructor].linked_inventory;
    let iron = g.data.group("Iron").unwrap();
    let size = g.inventories[&inv].size;
    for _ in 0..size { assert!(g.add_new_item(iron, inv, 0).0); }
    assert!(g.inventories[&inv].is_full());
    send_on(&mut g, 1, net, "SettingProxy.SetSettingServerRpc", &[json!(1)]);
    assert_eq!(g.wos[&destructor].setting, 1);
    let remaining = g.inventories[&inv].items.len();
    let item = g.inventories[&inv].items[0];
    send_scene(&mut g, 1, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(item), json!(inv), json!(false)]);
    assert!(g.add_existing_item(item, inv, true, 0));
    assert!(g.inventories[&inv].items.is_empty(), "control: removing/replacing one item makes it clean");
    assert_eq!(remaining, 0, "full enabled destructor kept {remaining} items until inventory changed");
}

#[test]
fn audit_picking_up_saved_item_does_not_increment_total_objects_again() {
    let (mut g, _) = world().expect("real game tables required");
    let (bag, _) = backpack(&g, 1);
    let iron = g.data.group("Iron").unwrap();
    let before = g.group_count(false, iron);
    let id = g.create_and_drop_on_floor(iron, [570.0, 3.0, 500.0], 0.0, true, false, 0.0, 1);
    assert_eq!(g.group_count(false, iron), before + 1);
    let hash = g.data.groups[iron].hash;
    send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(id), json!(hash), json!(bag), json!(true)]);
    assert!(g.inventories[&bag].items.contains(&id));
    assert_eq!(g.group_count(false, iron), before + 1, "one saved object must count once after pickup");
}
