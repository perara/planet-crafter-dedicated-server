use serde_json::{json, Value};

use super::back_and_forth::landing_step;
use crate::data::stable_hash;
use crate::game::{Args, Game};
use crate::net::VarValue;
use crate::save::SaveFile;

fn game(objects: Vec<Value>, inventories: Vec<Value>) -> Option<Game> {
    let mut save = SaveFile::empty();
    save.world = json!({"terraTokens": 0, "allTimeTerraTokens": 0, "unlockedGroups": ""});
    save.planets = vec![json!({"planetId": "Prime"}), json!({"planetId": "Humble"})];
    save.stats = json!({"craftedObjects": 0});
    save.settings = json!({"planetId": "Prime"});
    save.objects = objects;
    save.inventories = inventories;
    crate::game::test_game(save)
}

fn placed(id: i32, group: &str, position: &str, planet: &str, extra: Value) -> Value {
    let mut o = json!({"id": id, "gId": group, "pos": position, "rot": "0,0,0,1", "planet": stable_hash(planet)});
    o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    o
}

fn tokens(game: &Game) -> u64 {
    let (o, _) = game.net.find_class("TokensHandler").unwrap();
    match game.net.get(o, "TokensHandler", "_tokensNumber") {
        Some(VarValue::Plain(b)) => crate::wire::Reader::new(b).packed_u64().unwrap(),
        _ => panic!(),
    }
}

fn groups_in(game: &Game, inventory: i32) -> Vec<String> {
    game.inventories[&inventory].items.iter().map(|i| game.data.groups[game.wos[i].group].id.clone()).collect()
}

fn rpc(game: &mut Game, wo: i32, name: &str) -> bool {
    let net = game.wos[&wo].net.unwrap();
    super::rpc(game, 1, net, 0, name, &Args(vec![]))
}

/// Ticks every 5 ms (the server's pace) until `done` or `limit` seconds; returns the time taken.
fn run_until(game: &mut Game, limit: f32, done: impl Fn(&Game) -> bool) -> Option<f32> {
    let start = game.time;
    while game.time < start + limit {
        game.time += 0.005;
        super::tick(game);
        if done(game) {
            return Some(game.time - start);
        }
    }
    None
}

/// Names of the ClientRpcs queued since `from`.
fn sent(game: &Game, from: usize) -> Vec<String> {
    game.net.out[from..]
        .iter()
        .filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC)
        .filter_map(|(_, _, body)| {
            let mut r = crate::wire::Reader::new(body);
            r.packed_u64()?;
            r.packed_u16()?;
            Some(game.net.spec.get(r.packed_u32()?)?.name.clone())
        })
        .collect()
}

#[test]
fn departure_switches_planet_after_the_launch() {
    let (platform, rocket) = (210_000_040, 210_000_041);
    let Some(mut g) = game(vec![placed(platform, "DeparturePlatform", "300,10,300", "Prime", json!({})), placed(rocket, "RocketTravel1", "300,11,301", "Prime", json!({"liId": 70}))], vec![json!({"id": 70, "woIds": "", "size": 20})]) else { return };
    let net = g.wos[&platform].net.unwrap();
    let args = Args(vec![json!(super::INTERPLANETARY_TRAVEL), json!(stable_hash("Humble")), json!(0)]);
    let from = g.net.out.len();
    assert!(super::rpc(&mut g, 1, net, 1, "MachineDeparturePlatform.StartDepartureSequenceServerRpc", &args));
    // Once only.
    super::rpc(&mut g, 1, net, 1, "MachineDeparturePlatform.StartDepartureSequenceServerRpc", &args);
    assert_eq!(sent(&g, from), vec!["MachineDeparturePlatform.StartDepartureSequenceClientRpc"]);
    assert_eq!(g.ext.rockets.departures[&platform].rocket, Some(rocket));
    let from = g.net.out.len();
    let t = run_until(&mut g, 30.0, |g| g.ext.rockets.departures[&platform].switching).unwrap();
    assert!((t - 26.0).abs() < 0.01, "{t}");
    assert_eq!(sent(&g, from), vec!["MachineDeparturePlatform.SwitchPlanetClientRpc"]);
    // The switch itself a frame later, and the platform can start again afterwards.
    super::tick(&mut g);
    assert!(!g.ext.rockets.departures[&platform].started);
}

#[test]
fn departure_platform_is_free_again_once_the_ending_is_over() {
    let platform = 210_000_050;
    let Some(mut g) = game(vec![placed(platform, "DeparturePlatform", "300,10,300", "Prime", json!({}))], vec![]) else { return };
    let net = g.wos[&platform].net.unwrap();
    g.connect(1, None, false, None);
    let state = crate::game::PlayerState { id: 1, name: "a".into(), inventory: 0, equipment: 0, position: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], planet: String::new(), total_crafted: 0, total_tokens: 0, camera_view: 0, host: false, gauges: [100.0; 4] };
    g.clients.get_mut(&1).unwrap().ready = Some(state);
    // EndingRiley.
    let args = Args(vec![json!(1), json!(0), json!(0)]);
    let start = |g: &mut Game| super::rpc(g, 1, net, 1, "MachineDeparturePlatform.StartDepartureSequenceServerRpc", &args);
    let from = g.net.out.len();
    start(&mut g);
    run_until(&mut g, 120.0, |_| false);
    start(&mut g);
    assert_eq!(sent(&g, from).len(), 1, "refused while the ending plays");
    // Its end loads the intro scene, which leaves the session.
    g.disconnect(1);
    super::tick(&mut g);
    start(&mut g);
    assert_eq!(sent(&g, from), vec!["MachineDeparturePlatform.StartDepartureSequenceClientRpc"; 2]);
}

#[test]
fn landing_takes_half_a_minute() {
    // MachineRocketLand at 20 m/s from 500 m: 23.75 s flat out, then the 25 m deceleration.
    for dt in [1.0 / 30.0, 0.005] {
        let (mut height, mut t) = (500.0, 0.0);
        while let Some(h) = landing_step(height, 20.0, dt) {
            height = h;
            t += dt;
        }
        assert!((t - 30.6).abs() < 0.2, "{t}");
    }
}

#[test]
fn trade_rocket_sells_buys_and_comes_back() {
    let platform = 210_000_001;
    let Some(mut g) = game(
        vec![
            placed(platform, "TradePlatform1", "100,10,100", "Prime", json!({"liId": 50, "liGrps": "Bacteria1,Bacteria1,Bacteria1,Bacteria1,Bacteria1"})),
            json!({"id": 210_000_002, "gId": "astrofood2"}),
            json!({"id": 210_000_003, "gId": "astrofood2"}),
        ],
        vec![json!({"id": 50, "woIds": "210000002,210000003", "size": 25})],
    ) else { return };
    let (o, _) = g.net.find_class("TokensHandler").unwrap();
    assert!(matches!(g.net.get(o, "TokensHandler", "_isTradingActivated"), Some(VarValue::Plain(b)) if b == &vec![1]));
    assert!(rpc(&mut g, platform, "ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc"));
    assert_eq!(g.wos[&platform].growth, 1.0);
    // Away: a second send is refused.
    let before = g.net.out.len();
    rpc(&mut g, platform, "ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc");
    assert_eq!(g.net.out.len(), before + 1);
    let back = run_until(&mut g, 700.0, |g| g.wos[&platform].growth == 0.0).expect("the rocket comes back");
    // 99 growth steps of 6 s, then the landing.
    assert!((back - (99.0 * 6.0 + 30.6)).abs() < 1.5, "{back}");
    // Sold 2 x 20, bought Bacteria1 at 10 while the tokens lasted.
    assert_eq!(tokens(&g), 0);
    assert_eq!(groups_in(&g, 50), vec!["Bacteria1"; 4]);
    assert_eq!(g.wos[&platform].linked_groups, Some(vec![]));
    assert!(!g.wos.contains_key(&210_000_002));
}

#[test]
fn rocket_of_another_planet_lands_without_flying() {
    // Its GameObject is inactive: at step 99 the rocket is simply back.
    let platform = 210_000_005;
    let Some(mut g) = game(vec![placed(platform, "TradePlatform1", "100,10,100", "Humble", json!({"liId": 51, "grwth": 99}))], vec![json!({"id": 51, "woIds": "", "size": 25})]) else { return };
    let back = run_until(&mut g, 20.0, |g| g.wos[&platform].growth == 0.0).unwrap();
    assert!((back - 6.0).abs() < 0.01, "{back}");
}

#[test]
fn rocket_sent_to_space_multiplies_oxygen() {
    let (platform, rocket) = (210_000_010, 210_000_011);
    let Some(mut g) = game(vec![placed(platform, "LaunchPlatform", "200,10,200", "Prime", json!({})), placed(rocket, "RocketOxygen1", "200,11.5,200.5", "Prime", json!({}))], vec![]) else { return };
    let rocket_net = g.wos[&rocket].net.unwrap();
    assert!(rpc(&mut g, platform, "ConstructibleProxy.SendInSpaceServerRpc"));
    assert!(!g.wos[&rocket].is_placed());
    let space = g.data.group("SpaceMultiplierOxygen").unwrap();
    let multiplier = g.wos.values().find(|w| w.group == space).expect("a space multiplier").clone();
    assert_eq!(multiplier.position, [-500.0; 3]);
    assert_eq!(g.inventories[&multiplier.linked_inventory].items, vec![rocket]);
    assert_eq!(multiplier.unit_multiplier[crate::model::unit_index("Oxygen").unwrap()], 10.0);
    assert!(g.constructed.contains(&multiplier.id));
    // The rocket's network object goes 40 s after ignition.
    assert!(run_until(&mut g, 41.0, |g| !g.net.objects.contains_key(&rocket_net)).is_some_and(|t| t > 39.9));
    assert_eq!(g.wos[&rocket].net, None);
    // Nothing left on the platform: the error goes to the sender only.
    let before = g.net.out.len();
    rpc(&mut g, platform, "ConstructibleProxy.SendInSpaceServerRpc");
    assert_eq!(g.net.out.len(), before + 1);
    assert!(matches!(g.net.out.last(), Some((crate::net::Target::Only(1), _, _))));
}

#[test]
fn exchange_rocket_delivers_and_fetches_from_its_planet() {
    let (exchange, depot, container) = (210_000_020, 210_000_021, 210_000_022);
    let humble = stable_hash("Humble");
    let Some(mut g) = game(
        vec![
            placed(exchange, "InterplanetaryExchangePlatform1", "100,10,100", "Prime", json!({"liId": 60, "liPlanet": humble, "liGrps": "Iron,Iron"})),
            placed(depot, "PlanetaryDeliveryDepot1", "50,10,50", "Humble", json!({"liId": 61, "liGrps": "astrofood2"})),
            placed(container, "Container1", "60,10,60", "Humble", json!({"liId": 62})),
            json!({"id": 210_000_030, "gId": "astrofood2"}),
            json!({"id": 210_000_031, "gId": "Iron"}),
            json!({"id": 210_000_032, "gId": "Iron"}),
            json!({"id": 210_000_033, "gId": "Iron"}),
        ],
        vec![
            json!({"id": 60, "woIds": "210000030", "size": 25}),
            json!({"id": 61, "woIds": "", "size": 40}),
            json!({"id": 62, "woIds": "210000031,210000032,210000033", "size": 15}),
        ],
    ) else { return };
    let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
    let counts = |g: &Game| -> Vec<Vec<u8>> {
        let (o, _) = g.net.find_class("LogisticInterplanetaryManager").unwrap();
        match g.net.get(o, "LogisticInterplanetaryManager", "_itemsInPlanetaryLogisticsGroups") {
            Some(VarValue::List(items)) => items.clone(),
            _ => panic!("not a list"),
        }
    };
    let entry = |count: i32| [iron.to_le_bytes(), humble.to_le_bytes(), count.to_le_bytes()].concat();
    assert_eq!(counts(&g), vec![entry(3)]);
    super::add_speed_multiplier(&mut g, exchange, 9.0);
    assert!(rpc(&mut g, exchange, "ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc"));
    run_until(&mut g, 120.0, |g| g.wos[&exchange].growth == 0.0).expect("the rocket comes back");
    assert_eq!(groups_in(&g, 61), vec!["astrofood2"]);
    assert_eq!(groups_in(&g, 60), vec!["Iron", "Iron"]);
    assert_eq!(groups_in(&g, 62), vec!["Iron"]);
    // Two irons left Humble's logistics; the delivered food joined them.
    let food = g.data.groups[g.data.group("astrofood2").unwrap()].hash;
    let food_entry = [food.to_le_bytes(), humble.to_le_bytes(), 1i32.to_le_bytes()].concat();
    assert_eq!(counts(&g), vec![entry(1), food_entry]);
}
