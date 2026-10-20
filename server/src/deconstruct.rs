//! GroupNetworkBase.DeconstructServerRpc and ActionDeconstructible.CheckDeconstructOnServer:
//! taking a building apart gives its recipe back after the deconstruction animation.

use serde_json::json;

use crate::game::Game;
use crate::net::Target;

/// DeconstructStatus.
const OK: i32 = 0;

pub fn deconstruct(game: &mut Game, client: u64, object: u64, time: f32) {
    let Some(wo) = game.net.objects.get(&object).and_then(|o| o.wo) else { return };
    if game.ext.deconstructing.contains(&wo) || !crate::machines::deconstructible(game, wo) {
        return;
    }
    let status = check(game, wo);
    if status != OK {
        game.net.rpc(Target::Only(client), object, "GroupNetworkBase.DeconstructErrorClientRpc", &[json!(status)]);
        return;
    }
    game.ext.deconstructing.insert(wo);
    game.net.rpc(Target::All, object, "GroupNetworkBase.DeconstructClientRpc", &[json!(time)]);
    game.net.rpc(Target::Only(client), object, "GroupNetworkBase.StartDeconstructClientRpc", &[]);
    crate::scene::schedule(game, time, crate::scene::Timer::FinalDeconstruct(wo, client));
}

/// ActionDeconstructible.CheckDeconstructOnServer.
fn check(game: &Game, wo: i32) -> i32 {
    crate::machines::deconstruct_status(game, wo)
}

/// FinalDeconstructServer.
pub fn finish(game: &mut Game, wo: i32, client: u64) {
    game.ext.deconstructing.remove(&wo);
    let Some(w) = game.wos.get(&wo) else { return };
    let status = check(game, wo);
    if status != OK {
        if let Some(net) = w.net {
            game.net.rpc(Target::Only(client), net, "GroupNetworkBase.DeconstructErrorClientRpc", &[json!(status)]);
        }
        return;
    }
    let (hash, position) = (game.data.groups[w.group].hash, w.position);
    if let Some(net) = w.net {
        crate::panels::reset_contiguous_panels(game, net);
    }
    if crate::model::is_from_scene(wo) {
        game.set_save_state(wo, true);
    }
    let backpack = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.inventory).unwrap_or(-1);
    let position = game.wos[&wo].net.and_then(|n| game.net.objects.get(&n)).map(|o| o.position).unwrap_or(position);
    game.retrieve_resources_from_deconstruction(wo, hash, backpack, position, client);
    game.ext.plugins.actor = client;
    game.destroy_world_object(wo, true);
    game.ext.plugins.actor = 0;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::net::Target;
    use crate::testkit::*;

    /// Deconstructing: everyone sees the animation, the deconstructor starts it, a second
    /// request while it runs is ignored, and when it ends the building is gone and its recipe
    /// in the deconstructor's backpack. A container with something inside says why it cannot.
    #[test]
    fn deconstruction_takes_the_animations_time_then_gives_the_recipe_back() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let had = g.inventories[&bag].items.len();
        let (id, net) = place(&mut g, "EnergyGenerator1", [570.0, 3.0, 500.0]);
        g.net.out.clear();
        send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
        send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
        assert_eq!(sent(&g, "GroupNetworkBase.DeconstructClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::All, vec![json!(2.0)])], "once");
        assert_eq!(sent(&g, "GroupNetworkBase.StartDeconstructClientRpc").into_iter().map(|s| s.to).collect::<Vec<_>>(), vec![Target::Only(1)]);
        g.time += 1.0;
        crate::scene::tick(&mut g);
        assert!(g.wos.contains_key(&id), "still taking it apart");
        g.time += 1.5;
        crate::scene::tick(&mut g);
        assert!(!g.wos.contains_key(&id) && !g.net.objects.contains_key(&net));
        let recipe = g.data.groups[g.data.group("EnergyGenerator1").unwrap()].recipe.clone();
        assert_eq!(g.inventories[&bag].items[had..].iter().map(|i| g.wos[i].group).collect::<Vec<_>>(), recipe);
        assert!(saved_object(&mut g, id).is_none());

        let (chest, net) = place(&mut g, "Container1", [575.0, 3.0, 500.0]);
        let inside = g.wos[&chest].linked_inventory;
        give(&mut g, "Iron", inside);
        g.net.out.clear();
        send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
        assert_eq!(sent(&g, "GroupNetworkBase.DeconstructErrorClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!(crate::machines::INVENTORY_NOT_EMPTY)])]);
        assert!(sent(&g, "GroupNetworkBase.DeconstructClientRpc").is_empty());
    }

    #[test]
    fn deconstruction_rechecks_contents_before_completion() {
        let Some((mut g, _)) = crate::testkit::world() else { return };
        let (chest, net) = place(&mut g, "Container1", [570.0, 3.0, 500.0]);
        let inv = g.wos[&chest].linked_inventory;
        send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
        assert!(!sent(&g, "GroupNetworkBase.StartDeconstructClientRpc").is_empty());
        let iron = g.data.group("Iron").unwrap();
        let hash = g.data.groups[iron].hash;
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(hash), json!(inv)]);
        let cargo = g.inventories[&inv].items[0];
        let (bag, _) = backpack(&g, 1);
        let had = g.inventories[&bag].items.clone();
        g.net.out.clear();
        g.time += 3.0;
        crate::scene::tick(&mut g);
        assert!(g.wos.contains_key(&chest) && g.net.objects.contains_key(&net));
        assert_eq!(g.inventories[&inv].items, vec![cargo]);
        assert_eq!(g.inventories[&bag].items, had, "cancelled deconstruction refunds no recipe");
        assert!(!g.ext.deconstructing.contains(&chest));
        assert_eq!(one(&g, "GroupNetworkBase.DeconstructErrorClientRpc"), vec![json!(crate::machines::INVENTORY_NOT_EMPTY)]);
        g.inventory_remove(inv, cargo);
        send_on(&mut g, 1, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
        g.time += 3.0;
        crate::scene::tick(&mut g);
        assert!(!g.wos.contains_key(&chest), "emptying the chest allows a later attempt");
        assert!(g.wos.contains_key(&cargo));
    }

}
