//! Logistics review findings, each as a test.

use super::*;
use serde_json::json;

fn demand(g: &mut Game, inv: i32, group: &str) {
    let hash = g.data.groups[g.data.group(group).unwrap()].hash;
    g.inventories_rpc(1, "InventoriesHandler.DirtyInventoryLogisticsServerRpc", &Args(vec![json!(inv), json!([hash]), json!([]), json!(0)]));
}

fn run(g: &mut Game, t: &mut f64, ticks: usize) {
    for _ in 0..ticks {
        *t += 0.005;
        g.sim_time = Some(*t);
        g.tick();
        g.net.out.clear();
    }
}

/// M1: a container taken apart and another built, which gets its inventory id: drones serve the
/// new one (its logistic entity is its own, not the old building's).
#[test]
fn a_reused_inventory_id_gets_a_new_logistic_entity() {
    let Some(mut g) = crate::machines::tests::game() else { return };
    let mut t = 1.0;
    g.sim_time = Some(t);
    g.tick();
    crate::machines::tests::build(&mut g, "DroneStation1", [600.0, 5.0, 600.0]);
    let a = crate::machines::tests::build(&mut g, "Container2", [610.0, 5.0, 600.0]);
    let inv_a = g.wos[&a].linked_inventory;
    demand(&mut g, inv_a, "Iron");
    run(&mut g, &mut t, 400);
    assert_eq!(entity_wo(&g, inv_a), Some(a));
    g.destroy_world_object(a, true);
    let b = crate::machines::tests::build(&mut g, "Container2", [620.0, 5.0, 600.0]);
    let inv_b = g.wos[&b].linked_inventory;
    assert_eq!(inv_b, inv_a, "the id is used again");
    demand(&mut g, inv_b, "Vegetable0Growable");
    assert_eq!(entity_wo(&g, inv_b), Some(b));
    let veg = g.data.group("Vegetable0Growable").unwrap();
    let item = g.create_world_object(veg, 0, true);
    g.drop_on_floor(item, [615.0, 5.0, 605.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 0);
    run(&mut g, &mut t, 600);
    assert!(g.ext.logistics.all_tasks.contains_key(&item) || g.inventories[&inv_b].items.contains(&item), "a drone takes the vegetable to the new container");
}
