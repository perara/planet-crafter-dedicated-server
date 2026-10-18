//! Play loops across the systems: players join, craft, build, power and terraform, the world is
//! saved and loaded again, and what comes back is what was saved.

use serde_json::json;

use crate::game::Game;
use crate::testkit::*;
use crate::units::Kind;

/// Ticks the server loop's way (every 5 ms, on a fixed clock) for `seconds`.
fn play(g: &mut Game, seconds: f64) {
    let start = g.sim_time.unwrap_or(0.0);
    let mut t = start;
    while t < start + seconds {
        t += 0.005;
        g.sim_time = Some(t);
        g.tick();
        g.net.out.clear();
    }
}

fn total(g: &Game, kind: Kind) -> f64 {
    crate::units::value(g, kind, 0)
}

/// Two players: one crafts and builds a heater, which does nothing without power; the other
/// builds a generator, and the heat rises from then on. The save holds every part of it: the
/// buildings, the heat, the craft count, the players' backpacks. Loaded again (a restart), the
/// world saves the same text, a returning player gets their own backpack back, and the heat
/// goes on from where it was.
#[test]
fn players_build_power_and_terraform_and_a_restart_keeps_it_all() {
    let Some((mut g, _)) = world() else { return };
    g.sim_time = Some(0.0);
    join(&mut g, 2, "Bob");
    let (ann_bag, _) = backpack(&g, 1);
    let iron = hash(&g, "Iron");
    for _ in 0..3 {
        send_scene(&mut g, 1, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(ann_bag)]);
        send_scene(&mut g, 1, "WorldObjectsHandler.AddOneToTotalCraftServerRpc", &[]);
    }
    let crafted: Vec<i32> = g.inventories[&ann_bag].items.clone();

    let build = |g: &mut Game, client: u64, group: &str, at: [f32; 3]| {
        g.net.out.clear();
        let h = hash(g, group);
        send_scene(g, client, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(h), json!(at), json!([0, 0, 0, 1]), json!(false), json!(true), json!(true), json!(false)]);
        let net = one(g, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc")[0].as_u64().unwrap();
        g.net.objects[&net].wo.unwrap()
    };
    let heater = build(&mut g, 1, "Heater1", [570.0, 3.0, 500.0]);
    play(&mut g, 5.0);
    assert_eq!(total(&g, Kind::Heat), 0.0, "no power: no heat");
    assert!(!crate::energy::has_enough(&g, 0));
    let generator = build(&mut g, 2, "EnergyGenerator1", [575.0, 3.0, 500.0]);
    play(&mut g, 10.0);
    assert!(crate::energy::has_enough(&g, 0));
    let heat = total(&g, Kind::Heat);
    assert!(heat > 0.0, "powered: the heater heats");

    let text = g.snapshot().to_text();
    let save = crate::save::SaveFile::from_text(&text);
    assert_eq!(save.stats["craftedObjects"], json!(3));
    for id in [heater, generator] {
        assert!(save.objects.iter().any(|o| o["id"] == id), "building {id} saved");
    }
    let ann = save.players.iter().find(|p| p["name"] == "Ann").unwrap();
    let saved_bag = save.inventories.iter().find(|i| i["id"] == ann["inventoryId"]).unwrap();
    assert!(crafted.iter().all(|c| saved_bag["woIds"].as_str().unwrap().split(',').any(|s| s == c.to_string())));

    // The restart: the same world, saved the same way.
    let mut back = crate::game::test_game(save).unwrap();
    assert_eq!(back.snapshot().to_text(), text, "a loaded world saves what it was loaded from");
    assert!((total(&back, Kind::Heat) - heat).abs() < 1e-3);
    let ann = join(&mut back, 1, "Ann");
    let init = sent(&back, "PlayerMainController.InitPlayerClientRpc").into_iter().find(|s| s.object == ann).unwrap();
    assert_eq!(init.args[0], json!(ann_bag), "her own backpack");
    assert_eq!(back.inventories[&ann_bag].items, crafted);
    back.sim_time = Some(0.0);
    play(&mut back, 5.0);
    assert!(total(&back, Kind::Heat) > heat, "and it goes on");
}

/// Mining a scene rock, carrying the ore to a container, a story message and the tokens it
/// earns: the save keeps the rock gone, the ore in the container, the message and the tokens.
#[test]
fn what_players_take_carry_and_earn_is_saved() {
    let Some((mut g, _)) = world() else { return };
    let (bag, _) = backpack(&g, 1);
    let iron = hash(&g, "Iron");
    let rock = 101_000_123;
    send_scene(&mut g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(rock), json!(iron), json!(bag), json!(true)]);
    let (chest, _) = place(&mut g, "Container1", [565.0, 3.0, 495.0]);
    let chest_inventory = g.wos[&chest].linked_inventory;
    send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(bag), json!(chest_inventory), json!(rock)]);
    send_scene(&mut g, 1, "TokensHandler.GainTokensServerRpc", &[json!(25)]);
    let message = g.data.assets.iter().find(|(k, _)| k.starts_with("MessageData:")).and_then(|(_, v)| v["stringId"].as_str()).unwrap().to_string();
    send_scene(&mut g, 1, "MessagesHandler.AddNewReceivedMessageServerRpc", &[json!(crate::data::stable_hash(&message)), json!(false)]);
    g.disconnect(1);

    let mut back = reload(&mut g);
    assert_eq!(back.inventories[&chest_inventory].items, vec![rock]);
    assert!(!back.wos[&rock].dont_save, "the rock stays mined");
    let saved = back.snapshot();
    assert_eq!(saved.world["terraTokens"], json!(25));
    assert!(saved.messages.iter().any(|m| m["stringId"] == json!(message)));
    join(&mut back, 3, "Carl");
    back.net.out.clear();
    send_scene(&mut back, 3, "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc", &[json!(rock)]);
    assert_eq!(one(&back, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc")[0], json!(rock), "a joining client removes the mined rock");
}
