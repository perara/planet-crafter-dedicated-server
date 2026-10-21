//! The cookbook: every example plugin of `examples/plugins/` installed as `pcds plugin install` and
//! an approval would, started in the real game (game tables needed; the tests skip without them) and
//! driven through the game's own paths: chat RPCs, joins, leaves, real build and deconstruct RPCs,
//! the game's clock. What is asserted is what the real game did: chat lines, kicks, the items in the
//! real backpack, the Terra tokens, the rules, refused RPCs, the event stream.
//!
//! Time is the game's: `run_to` steps it in quarter seconds (the game goes on by a third of a second
//! at most per frame). The wall clock is the real one, so what depends on it (restart-notice, the
//! "last seen" of playtime-board) is set from it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::game_tests::{chat_out, example, install_example, start};
use super::tests::fresh_dir;
use super::*;
use crate::game::Game;
use crate::net::Target;
use crate::testkit::{backpack, give, hash, join, send_on, send_scene, sent, var, world};

// ---- The rig.

/// A game with the plugins installed (as `install_example` does), their settings set (checked against
/// the manifest's declaration, as `pcds plugin config set` does) and started. None without game tables.
fn rig(name: &str, plugins: &[&str], settings: &[(&str, &str, &str)]) -> Option<(Game, PathBuf)> {
    rig_with(name, plugins, settings, false)
}

/// The same; `offline` keeps the HTTP requests of the plugins instead of sending them.
fn rig_with(name: &str, plugins: &[&str], settings: &[(&str, &str, &str)], offline: bool) -> Option<(Game, PathBuf)> {
    let (mut g, _) = world()?;
    let dir = fresh_dir(name);
    for plugin in plugins {
        install_example(&dir, plugin, None);
    }
    for (plugin, key, value) in settings {
        let manifest = read_folder(&dir.join(plugin)).manifest.unwrap_or_else(|| panic!("{plugin}: no manifest"));
        let declared = manifest.config.iter().find(|k| k.key == *key).unwrap_or_else(|| panic!("{plugin} declares no setting {key}"));
        let text = declared.check(value).unwrap_or_else(|e| panic!("{e}"));
        config::set(&dir, plugin, key, Some(&text)).unwrap();
    }
    if offline {
        let mut host = prepare(&dir, "world");
        host.offline = Some(Vec::new());
        attach(&mut g, host, false);
    } else {
        start(&mut g, &dir);
    }
    let host = g.ext.plugins.host.as_ref().expect("a host");
    assert!(host.plugins.iter().all(|p| p.running()), "{:?}", host.plugins.iter().map(|p| (&p.id, &p.status)).collect::<Vec<_>>());
    Some((g, dir))
}

fn now(g: &Game) -> f64 {
    g.sim_time.unwrap_or(0.0)
}

/// One frame of a quarter second: the game's clock, the plugins' rate limits (the host counts them on
/// the clock of the process, which a test that runs the game ahead of it must carry along), a tick.
fn frame(g: &mut Game) {
    let t = now(g) + 0.25;
    g.sim_time = Some(t);
    if let Some(host) = g.ext.plugins.host.as_mut() {
        for p in host.plugins.iter_mut() {
            if let Some(rt) = p.rt.as_mut() {
                rt.ctx().skew += std::time::Duration::from_millis(250);
            }
        }
    }
    g.tick();
}

/// Frames of a quarter second until the game's clock reads `end`.
fn run_to(g: &mut Game, end: f64) {
    while now(g) < end {
        frame(g);
    }
}

fn run_for(g: &mut Game, seconds: f64) {
    run_to(g, now(g) + seconds);
}

/// Like `run_to`, and everything said in the chat on the way: (to, text). The output is emptied at each frame.
fn run_chat(g: &mut Game, end: f64) -> Vec<(Target, String)> {
    g.net.out.clear();
    let mut all = Vec::new();
    while now(g) < end {
        frame(g);
        all.extend(chat_out(g));
        g.net.out.clear();
    }
    all
}

/// A question to the chat after two seconds of the game's time (its cooldowns count it, and the
/// game's chat limit of one message a second); what was told to the asker.
fn talk(g: &mut Game, client: u64, text: &str) -> Vec<String> {
    run_for(g, 2.0);
    chat(g, client, text).into_iter().filter(|(to, _)| *to == Target::Only(client)).map(|(_, t)| t).collect()
}

/// What was said to everyone (the server's `say`).
fn said(lines: &[(Target, String)]) -> Vec<&str> {
    lines.iter().filter(|(to, _)| *to == Target::All).map(|(_, t)| t.as_str()).collect()
}

/// What was told to one client.
fn told(lines: &[(Target, String)], client: u64) -> Vec<&str> {
    lines.iter().filter(|(to, _)| *to == Target::Only(client)).map(|(_, t)| t.as_str()).collect()
}

/// A chat message of `client`, as the game receives it; what the server sent back at once.
fn chat(g: &mut Game, client: u64, text: &str) -> Vec<(Target, String)> {
    g.net.out.clear();
    send_scene(g, client, "ChatHandler.SendChatMessageServerRpc", &[json!(text)]);
    chat_out(g)
}

/// A chat message, then one tick: the server's `say` goes out at the tick. Returns what everyone heard.
fn chat_and_tick(g: &mut Game, client: u64, text: &str) -> Vec<(Target, String)> {
    g.net.out.clear();
    send_scene(g, client, "ChatHandler.SendChatMessageServerRpc", &[json!(text)]);
    frame(g);
    chat_out(g)
}

/// The Terra tokens of the world.
fn tokens(g: &Game) -> u64 {
    let (object, _) = g.net.find_class("TokensHandler").unwrap();
    crate::wire::Reader::new(&var(g, object, "TokensHandler", "_tokensNumber")).packed_u64().unwrap_or(0)
}

/// How many items of a group a client's backpack holds.
fn count(g: &Game, client: u64, group: &str) -> usize {
    let (bag, _) = backpack(g, client);
    g.inventories[&bag].items.iter().filter(|i| g.data.groups[g.wos[*i].group].id == group).count()
}

/// Makes a client an operator of the server (a verified SteamID on the operators list).
fn operator(g: &mut Game, client: u64, steam_id: u64) {
    g.clients.get_mut(&client).unwrap().verified_steam_id = Some(steam_id);
    g.access.lists.operators.push(steam_id.to_string());
}

/// The player's body moves in the world.
fn move_to(g: &mut Game, client: u64, position: [f32; 3]) {
    let body = g.clients[&client].player.unwrap();
    g.net.move_object(body, position, None);
}

/// A poll of the event stream as an integration makes it; the first poll of a consumer registers it.
fn poll(g: &mut Game, consumer: &str, types: &[&str], after: u64) -> Value {
    let (status, body) = admin::request(g, "POST", "/api/plugins/events", &json!({"consumer": consumer, "types": types, "after": after}));
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn plugin<'a>(g: &'a Game, id: &str) -> &'a Loaded {
    g.ext.plugins.host.as_ref().unwrap().plugins.iter().find(|p| p.id == id).unwrap_or_else(|| panic!("no plugin {id}"))
}

/// A hot update as the bank does it: the states of the plugins into the snapshot, a new game restored
/// from it, the same folder started in it with the flip announced. Returns the new game.
fn hot_update(g: &mut Game, dir: &Path) -> Game {
    // (What the game queued for the clients has gone out by then.)
    g.net.out.clear();
    checkpoint(g, Some(limits::FLIP_BUDGET));
    let snapshot = crate::handoff::snapshot(g).unwrap();
    let dirs = crate::test_data().unwrap();
    let (n, d, s) = crate::tables(dirs);
    let scenes = crate::scene_tables(dirs, &n, &d);
    let mut next = crate::handoff::restore(n, d, s, &snapshot, scenes).unwrap();
    crate::handoff::verify(&next, &snapshot).unwrap();
    attach(&mut next, prepare(dir, "world"), true);
    next
}

/// The answer of the webhook's server to the request with this ticket: 204, no content.
fn answer_http(g: &mut Game, ticket: u64) {
    let mut host = g.ext.plugins.host.take().unwrap();
    let ms = (g.time.secs() * 1000.0) as i64;
    host.send(0, g, &Event::HttpDone { ticket, status: 204, error: "", body: &[], headers: Vec::new() }, ms);
    g.ext.plugins.host = Some(host);
}

/// A real build request of `client`: the network object it was given, None if it was refused. The request
/// is made as the game client makes it (WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc),
/// with the check off.
fn build(g: &mut Game, client: u64, group: &str, position: [f32; 3]) -> Option<u64> {
    g.net.out.clear();
    let hash = hash(g, group);
    send_scene(g, client, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(hash), json!(position), json!([0.0, 0.0, 0.0, 1.0]), json!(true), json!(false), json!(true), json!(false)]);
    // A request a plugin refused is not answered at all.
    sent(g, "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc").first().and_then(|s| s.args[0].as_u64()).filter(|n| *n != 0)
}

/// A real deconstruct request of `client` for the object with network id `net`, and the animation's time.
fn take_apart(g: &mut Game, client: u64, net: u64) {
    g.net.out.clear();
    send_on(g, client, net, "GroupNetworkBase.DeconstructServerRpc", &[json!(2.0)]);
    run_for(g, 2.5);
}

// ---- Data plugins.

/// welcome-rules: a join is welcomed, commands answer with their cooldown, a leave is told, and the
/// announcement comes every 15 minutes of game time.
#[test]
fn welcome_rules_greets_answers_and_announces() {
    let Some((mut g, _)) = rig("cb-welcome", &["welcome-rules"], &[]) else { return };
    let link = "https://discord.gg/your-invite";
    // A join: the welcome to everyone, the hint to the new player alone.
    join(&mut g, 2, "Bo");
    g.tick();
    let out = chat_out(&g);
    assert_eq!(said(&out), ["Welcome, Bo! 2 online."]);
    assert_eq!(told(&out, 2), ["Type !rules for the rules."]);
    // A command answers the asker alone and has a cooldown of 5 s (ask() lets 2 s of game time pass).
    assert_eq!(talk(&mut g, 2, "!rules"), [format!("1. Be kind. 2. Do not take what others built. 3. Ask an admin when unsure. Chat: {link}")]);
    assert!(talk(&mut g, 2, "!rules").is_empty(), "2 s later: the cooldown holds");
    run_for(&mut g, 4.0);
    assert_eq!(talk(&mut g, 2, "!rules").len(), 1, "6 s later: it answers again");
    assert_eq!(talk(&mut g, 1, "!discord"), [link]);
    // !online is said to everyone.
    let out = chat_and_tick(&mut g, 1, "!online");
    assert!(said(&out).iter().any(|t| t.starts_with("Ann asks: 2 players on Prime after ")), "{out:?}");
    // A leave is told to the others.
    g.net.out.clear();
    g.disconnect(2);
    g.tick();
    assert_eq!(said(&chat_out(&g)), ["Bo left."]);
    // Every 15 minutes the community link is said; the clock starts at the first tick.
    let out = run_chat(&mut g, 1000.0);
    let announcements = said(&out).iter().filter(|t| **t == format!("Join the community: {link}")).count();
    assert_eq!(announcements, 1, "{out:?}");
}

/// server-motd: three lines for a joining player alone, the commands, tips that rotate every 10 minutes
/// and nothing said to an empty server.
#[test]
fn server_motd_tells_the_day_and_rotates_its_tips() {
    let Some((mut g, _)) = rig("cb-motd", &["server-motd"], &[]) else { return };
    let link = "https://discord.gg/your-invite";
    join(&mut g, 2, "Bo");
    g.tick();
    let out = chat_out(&g);
    assert!(said(&out).is_empty(), "nothing to everyone: {out:?}");
    let today = format!("Today: the new base area is open east of the landing pod. Chat and events: {link}");
    assert_eq!(told(&out, 2), ["Welcome to the server, Bo! 2 online on Prime.", "!rules has the rules, !motd repeats this, !help lists the commands.", today.as_str()]);
    assert_eq!(talk(&mut g, 2, "!help"), [format!("Commands: !motd !rules !help !online. More on {link}")]);
    assert_eq!(talk(&mut g, 2, "!rules"), ["1. Be kind. 2. Do not take apart what others built. 3. ask in the chat or on Discord when unsure."]);
    assert_eq!(talk(&mut g, 2, "!motd").len(), 1);
    assert!(talk(&mut g, 2, "!motd").is_empty(), "the cooldown of 10 s holds");
    let out = chat_and_tick(&mut g, 1, "!online");
    assert!(said(&out).iter().any(|t| t.starts_with("Ann asks: 2 players on Prime after ")), "{out:?}");
    // Tips rotate every 10 minutes (from the first tick); at 30 minutes the third tip and the community link.
    let out = run_chat(&mut g, 1830.0);
    let heard = said(&out);
    assert_eq!(heard.len(), 4, "{heard:?}");
    assert!(heard[0].starts_with("Tip: a Container") && heard[1].starts_with("Tip: water and oxygen"), "{heard:?}");
    assert!(heard[2..].iter().any(|t| t.starts_with("Tip: the terraformation index")) && heard[2..].contains(&format!("Join the community: {link}").as_str()), "{heard:?}");
    // Nobody online (the world keeps its clock for the test): nothing is said when the next tip is due.
    g.rules.world.pause_when_empty = false;
    g.disconnect(1);
    g.disconnect(2);
    let out = run_chat(&mut g, 2550.0);
    assert!(said(&out).is_empty(), "an empty server hears nothing: {out:?}");
}

/// scaling-rules: the speed rules follow the number of players online, one change when the band
/// changes, and each change is in the event stream.
#[test]
fn scaling_rules_follow_the_players_online() {
    let Some((mut g, _)) = rig("cb-scaling", &["scaling-rules"], &[]) else { return };
    poll(&mut g, "watch", &["rule_change"], 0);
    let speeds = |g: &Game| (g.rules.speed.growth, g.rules.speed.machines);
    run_for(&mut g, 2.0);
    assert_eq!(speeds(&g), (2.0, 1.5), "one player");
    run_for(&mut g, 30.0);
    for c in 2..=3 {
        join(&mut g, c, &format!("P{c}"));
    }
    run_for(&mut g, 2.0);
    assert_eq!(speeds(&g), (1.5, 1.25), "three players");
    for c in 4..=6 {
        join(&mut g, c, &format!("P{c}"));
    }
    run_for(&mut g, 2.0);
    assert_eq!(speeds(&g), (1.0, 1.0), "six players: the game's own speed");
    assert_eq!(talk(&mut g, 1, "!tier"), ["6 online. Plants grow 2x as fast with 1-2 players, 1.5x with 3-5, at the game's speed from 6."]);
    for c in 4..=6 {
        g.disconnect(c);
    }
    run_for(&mut g, 2.0);
    assert_eq!(speeds(&g), (1.5, 1.25));
    for c in 2..=3 {
        g.disconnect(c);
    }
    run_for(&mut g, 2.0);
    assert_eq!(speeds(&g), (2.0, 1.5));
    // One event for each change of band (1 -> 3 -> 6 -> 3 -> 1, and the first), none while the band stays.
    let answer = poll(&mut g, "watch", &["rule_change"], 0);
    let changes: Vec<&Value> = answer["events"].as_array().unwrap().iter().collect();
    assert_eq!(changes.len(), 5, "{answer}");
    assert_eq!((changes[0]["changes"]["speed.growth"].as_f64(), changes[2]["changes"]["speed.growth"].as_f64()), (Some(2.0), None), "{answer}");
    assert_eq!(plugin(&g, "scaling-rules").stats.faults, 0);
}

// ---- hello-c and hello-stats.

/// hello-c (a C module, no libc): !hello counts across a hot update, !roll rolls a die, arguments
/// are refused by the host.
#[test]
fn hello_c_counts_rolls_and_survives_a_hot_update() {
    let Some((mut g, dir)) = rig("cb-hello-c", &["hello-c"], &[]) else { return };
    assert_eq!(talk(&mut g, 1, "!hello"), ["Hello Ann from C! I have said hello 1 time."]);
    join(&mut g, 2, "Bo");
    run_for(&mut g, 35.0);
    for client in [1, 2] {
        let rolled = talk(&mut g, client, "!roll");
        let name = if client == 1 { "Ann" } else { "Bo" };
        let die = rolled[0].strip_prefix(&format!("{name} rolled a ")).and_then(|r| r.strip_suffix('.')).and_then(|d| d.parse::<u32>().ok());
        assert!(die.is_some_and(|d| (1..=6).contains(&d)), "{rolled:?}");
    }
    assert_eq!(talk(&mut g, 1, "!roll 20"), ["Use: !roll"]);
    // The state goes through a hot update: the counter goes on.
    let mut next = hot_update(&mut g, &dir);
    assert_eq!(talk(&mut next, 1, "!hello"), ["Hello Ann from C! I have said hello 2 times."]);
    assert_eq!(plugin(&next, "hello-c").stats.faults, 0);
}

/// hello-stats: !stats reads the real world, !uses counts across a hot update.
#[test]
fn hello_stats_reads_the_world_and_keeps_its_counters() {
    let Some((mut g, dir)) = rig("cb-hello-stats", &["hello-stats"], &[]) else { return };
    join(&mut g, 2, "Bo");
    run_for(&mut g, 90.0);
    let stats = talk(&mut g, 1, "!stats");
    assert!(stats[0].starts_with("Server on Prime: 2 players, terraformation "), "{stats:?}");
    assert!(stats[0].contains("world time 0h 01m"), "{stats:?}");
    assert_eq!(talk(&mut g, 2, "!uses"), ["!stats was used 1 times, !uses 1 times on this world"]);
    let mut next = hot_update(&mut g, &dir);
    assert_eq!(talk(&mut next, 1, "!uses"), ["!stats was used 1 times, !uses 2 times on this world"]);
}

// ---- afk-guard.

/// afk-guard: a player who stands still is warned, then kicked; one who moves and an operator are not.
#[test]
fn afk_guard_warns_then_kicks_the_idle_and_spares_movers_and_operators() {
    let Some((mut g, _)) = rig("cb-afk", &["afk-guard"], &[("afk-guard", "afk_minutes", "1"), ("afk-guard", "afk_warn_seconds", "10")]) else { return };
    for (client, name) in [(2, "Bo"), (3, "Cy"), (4, "Root")] {
        join(&mut g, client, name);
    }
    operator(&mut g, 4, 76561198000000004);
    let mut heard = run_chat(&mut g, 44.0);
    assert!(g.kicks.is_empty() && heard.is_empty(), "{heard:?}");
    // Cy walks away from where he stood; the others stand still.
    move_to(&mut g, 3, [50.0, 0.0, 0.0]);
    heard.extend(run_chat(&mut g, 58.0));
    let warning = |lines: &[(Target, String)], client| told(lines, client).iter().filter(|t| t.starts_with("You seem to be away. Move within ")).count();
    assert_eq!((warning(&heard, 1), warning(&heard, 2), warning(&heard, 3), warning(&heard, 4)), (1, 1, 0, 0), "{heard:?}");
    assert!(told(&heard, 1)[0].ends_with(" seconds or you will be removed from the server."), "{heard:?}");
    assert!(g.kicks.is_empty());
    // A minute of standing still: Ann and Bo go, with the reason; Cy and the operator stay.
    run_to(&mut g, 66.0);
    let kicked: Vec<u64> = g.kicks.iter().map(|(c, _)| *c).collect();
    assert_eq!(kicked, [1, 2], "{:?}", g.kicks);
    assert!(g.kicks[0].1.starts_with("Removed for being away from the keyboard (1m 0") && g.kicks[0].1.ends_with("You are welcome to rejoin."), "{:?}", g.kicks);
    g.disconnect(1);
    g.disconnect(2);
    // Cy is idle again from his walk (t = 44) and is kicked a minute after it, not before; he moves once more.
    run_to(&mut g, 90.0);
    move_to(&mut g, 3, [0.0, 0.0, 70.0]);
    run_to(&mut g, 130.0);
    assert_eq!(g.kicks.len(), 2, "Cy moved at 90 and is not idle for a minute yet: {:?}", g.kicks);
    // !guard is for operators: the host answers the others, the operator gets the settings and the lists.
    assert_eq!(talk(&mut g, 3, "!guard"), ["!guard is for the server's operators."]);
    let guard = talk(&mut g, 4, "!guard");
    assert_eq!(guard[0], "Guard: away after 1 min (warning 10 s before); grief: more than 25 destroyed in 60 s (warn).");
    assert_eq!(guard[2], "Strikes: none.");
    assert!(guard[1].starts_with("Idle:"), "{guard:?}");
    // The operator is never kicked, however long he stands there.
    run_to(&mut g, 400.0);
    assert!(g.kicks.iter().all(|(c, _)| *c != 4), "{:?}", g.kicks);
    assert_eq!(plugin(&g, "afk-guard").stats.faults, 0);
}

/// afk-guard's grief guard: real build and deconstruct requests; the events carry the right client,
/// and the destroying player (not an operator) gets a strike, a warning, and the operators are told.
#[test]
fn afk_guard_counts_the_objects_a_player_takes_apart() {
    let settings = [("afk-guard", "afk_minutes", "240"), ("afk-guard", "destroy_limit", "3"), ("afk-guard", "destroy_window_seconds", "60")];
    let Some((mut g, _)) = rig("cb-grief", &["afk-guard"], &settings) else { return };
    join(&mut g, 3, "Cy");
    join(&mut g, 4, "Root");
    operator(&mut g, 4, 76561198000000004);
    poll(&mut g, "watch", &["object_placed", "object_destroyed"], 0);
    // Cy builds five generators with the game's own request and takes them apart one by one.
    let mut nets = Vec::new();
    for i in 0..5 {
        nets.push(build(&mut g, 3, "EnergyGenerator1", [600.0 + 10.0 * i as f32, 3.0, 500.0]).expect("built"));
    }
    run_for(&mut g, 1.0);
    let mut heard = Vec::new();
    for (i, net) in nets.iter().enumerate() {
        g.net.out.clear();
        take_apart(&mut g, 3, *net);
        heard.extend(chat_out(&g));
        if i == 2 {
            assert!(told(&heard, 4).is_empty() && told(&heard, 3).is_empty(), "three are allowed: {heard:?}");
        }
    }
    // The events of the real game name the client that built and that took apart.
    let answer = poll(&mut g, "watch", &["object_placed", "object_destroyed"], 0);
    let events = answer["events"].as_array().unwrap();
    let by = |kind: &str| events.iter().filter(|e| e["type"] == kind && e["group"] == "EnergyGenerator1").map(|e| e["client"].as_u64()).collect::<Vec<_>>();
    assert_eq!((by("object_placed"), by("object_destroyed")), (vec![Some(3); 5], vec![Some(3); 5]), "{answer}");
    // The fourth inside a minute is over the limit: the player is told, and the operator.
    assert_eq!(told(&heard, 4), ["Guard: Cy destroyed 4 objects in 60 s (strike 1). Action: warned."], "{heard:?}");
    assert_eq!(told(&heard, 3), ["Guard: you took apart 4 objects in 60 s. Slow down; the operators have been told."], "{heard:?}");
    assert!(g.kicks.is_empty());
    assert_eq!(talk(&mut g, 4, "!guard")[2], "Strikes: Cy 1");
    // An operator takes things apart as he likes.
    let mut lines = Vec::new();
    for i in 0..5 {
        let net = build(&mut g, 4, "EnergyGenerator1", [600.0 + 10.0 * i as f32, 3.0, 520.0]).expect("built");
        g.net.out.clear();
        take_apart(&mut g, 4, net);
        lines.extend(chat_out(&g));
    }
    assert!(lines.is_empty(), "{lines:?}");
    // !pardon: operators only, found by the start of the name.
    assert_eq!(talk(&mut g, 3, "!pardon Cy"), ["!pardon is for the server's operators."]);
    assert_eq!(talk(&mut g, 4, "!pardon c"), ["Pardoned Cy: 1 strike cleared."]);
    assert_eq!(talk(&mut g, 4, "!guard")[2], "Strikes: none.");
    assert_eq!(plugin(&g, "afk-guard").stats.faults, 0);

    // The same with grief_action = kick: the player is removed.
    let settings = [("afk-guard", "afk_minutes", "240"), ("afk-guard", "destroy_limit", "3"), ("afk-guard", "grief_action", "kick")];
    let Some((mut g, _)) = rig("cb-grief-kick", &["afk-guard"], &settings) else { return };
    join(&mut g, 3, "Cy");
    for i in 0..4 {
        let net = build(&mut g, 3, "EnergyGenerator1", [600.0 + 10.0 * i as f32, 3.0, 500.0]).expect("built");
        take_apart(&mut g, 3, net);
    }
    assert_eq!(g.kicks, [(3, "Removed by the grief guard: 4 objects destroyed within 60 seconds.".to_string())]);
}

// ---- playtime-board.

fn minutes_of(line: &str) -> i64 {
    // "1. Ann 0h 20m": the last two words.
    let words: Vec<&str> = line.split_whitespace().collect();
    let (h, m) = (words[words.len() - 2], words[words.len() - 1]);
    h.trim_end_matches('h').parse::<i64>().unwrap() * 60 + m.trim_end_matches('m').parse::<i64>().unwrap()
}

/// playtime-board: minutes counted from the game's clock while online, !top, !playtime, and the board
/// through a hot update.
#[test]
fn playtime_board_counts_game_time_and_survives_a_hot_update() {
    let Some((mut g, dir)) = rig("cb-playtime", &["playtime-board"], &[]) else { return };
    join(&mut g, 2, "Bo");
    join(&mut g, 3, "Cy");
    run_to(&mut g, 300.0);
    g.disconnect(3);
    run_to(&mut g, 600.0);
    g.disconnect(2);
    run_to(&mut g, 1200.0);
    let top = talk(&mut g, 1, "!top");
    assert_eq!(top.len(), 3, "{top:?}");
    for (line, (rank, name, minutes)) in top.iter().zip([(1, "Ann", 20), (2, "Bo", 10), (3, "Cy", 5)]) {
        assert!(line.starts_with(&format!("{rank}. {name} ")), "{top:?}");
        assert!((minutes_of(line) - minutes).abs() <= 1, "{top:?}");
    }
    assert_eq!(talk(&mut g, 1, "!top 2").len(), 2);
    assert_eq!(talk(&mut g, 1, "!top lots"), ["Use: !top [number from 1 to 10]"]);
    let own = talk(&mut g, 1, "!playtime");
    assert!(own[0].starts_with("You have played 0h 2") || own[0].starts_with("You have played 0h 19m"), "{own:?}");
    assert!(own[0].ends_with("(rank 1 of 3)."), "{own:?}");
    // The board is in the state a hot update carries.
    let mut next = hot_update(&mut g, &dir);
    let bo = talk(&mut next, 1, "!playtime bo");
    assert!(bo[0].starts_with("Bo: 0h 09m played, last seen ") || bo[0].starts_with("Bo: 0h 10m played, last seen "), "{bo:?}");
    let ann = talk(&mut next, 1, "!playtime Ann");
    assert!(ann[0].starts_with("Ann: 0h 2") || ann[0].starts_with("Ann: 0h 19m"), "{ann:?}");
    assert_eq!(talk(&mut next, 1, "!playtime nobody"), ["No player named nobody is online or on the board."]);
    assert_eq!(plugin(&next, "playtime-board").stats.faults, 0);
}

// ---- quest-rewards.

/// quest-rewards: the first building gives an item to the real backpack and tokens to the world; a full
/// backpack holds the items back; the operator's !givetokens; the host's rate limit on tokens.
#[test]
fn quest_rewards_give_items_and_tokens_for_real_buildings() {
    let Some((mut g, dir)) = rig("cb-quest", &["quest-rewards"], &[("quest-rewards", "world_milestones", "999999999999"), ("quest-rewards", "item_count", "3")]) else { return };
    join(&mut g, 2, "Bo");
    join(&mut g, 4, "Root");
    operator(&mut g, 4, 76561198000000004);
    run_for(&mut g, 1.0);
    // Ann's backpack in the test world has five free slots; she empties it.
    let (bag, _) = backpack(&g, 1);
    for id in g.inventories[&bag].items.clone() {
        g.inventory_remove(bag, id);
    }
    let (iron, coins) = (count(&g, 1, "Iron"), tokens(&g));
    // Ann's first real building: ten Iron in her backpack, a hundred tokens for the world, and a line.
    g.net.out.clear();
    build(&mut g, 1, "EnergyGenerator1", [600.0, 3.0, 500.0]).expect("built");
    assert_eq!((count(&g, 1, "Iron"), tokens(&g)), (iron + 3, coins + 100));
    assert_eq!(told(&chat_out(&g), 1), ["Quest reward for your first building: +3 Iron, +100 Terra tokens."]);
    // The next nine give nothing until the tenth.
    for i in 1..9 {
        build(&mut g, 1, "EnergyGenerator1", [600.0 + 10.0 * i as f32, 3.0, 500.0]).expect("built");
    }
    assert_eq!((count(&g, 1, "Iron"), tokens(&g)), (iron + 3, coins + 100));
    g.net.out.clear();
    build(&mut g, 1, "EnergyGenerator1", [700.0, 3.0, 500.0]).expect("built");
    assert_eq!((count(&g, 1, "Iron"), tokens(&g)), (iron + 6, coins + 200));
    assert!(told(&chat_out(&g), 1)[0].starts_with("Quest reward for 10 buildings placed"));
    let rewards = talk(&mut g, 1, "!rewards");
    assert!(rewards[0].starts_with("You have placed 10 buildings and played 0h 0"), "{rewards:?}");
    assert!(rewards[0].ends_with("Milestones done: 2 of 6."), "{rewards:?}");
    // An item dropped on the floor is an object placed by the client too (the cookbook's first run found the plugin counting it as a
    // building); builds are counted from the build RPC, so it does not count.
    let (bag, _) = backpack(&g, 1);
    let ore = give(&mut g, "Iron", bag);
    g.inventory_remove(bag, ore);
    send_scene(&mut g, 1, "WorldObjectsHandler.DropOnFloorServerRpc", &[json!(ore), json!([561.0, 3.0, 497.0]), json!(0.5), json!(true), json!(false), json!(0), json!(0.0)]);
    let rewards = talk(&mut g, 1, "!rewards");
    assert!(rewards[0].starts_with("You have placed 10 buildings"), "an item dropped on the floor is no building: {rewards:?}");

    // A full backpack: Bo's tokens come, his items wait until there is room.
    let (bag, _) = backpack(&g, 2);
    let cobalt = g.data.group("Cobalt").unwrap();
    let mut stuffed = Vec::new();
    loop {
        let id = g.create_world_object(cobalt, 0, true);
        if !g.inventory_add(bag, id, true) {
            break;
        }
        stuffed.push(id);
    }
    let coins = tokens(&g);
    g.net.out.clear();
    build(&mut g, 2, "EnergyGenerator1", [800.0, 3.0, 500.0]).expect("built");
    assert_eq!((count(&g, 2, "Iron"), tokens(&g)), (0, coins + 100), "the tokens are not held back");
    let out = chat_out(&g);
    assert!(told(&out, 2).iter().any(|t| t.contains("reward is waiting") && t.contains("free slots")), "{:?}", told(&out, 2));
    assert!(talk(&mut g, 2, "!rewards")[0].ends_with("1 reward is waiting; it is given when the backpack has room."));
    run_for(&mut g, 20.0);
    assert_eq!(count(&g, 2, "Iron"), 0);
    for id in stuffed {
        g.inventory_remove(bag, id);
    }
    g.net.out.clear();
    let end = now(&g) + 6.0;
    let heard = run_chat(&mut g, end);
    assert_eq!(count(&g, 2, "Iron"), 3, "given at the next tick after there is room");
    assert_eq!(told(&heard, 2), ["Quest reward for your first building: +3 Iron, +100 Terra tokens."]);
    assert_eq!(tokens(&g), coins + 100, "the tokens were paid once");

    // !givetokens: the host turns away a player who is no operator; an operator adds tokens, with limits.
    assert_eq!(talk(&mut g, 2, "!givetokens 5"), ["!givetokens is for the server's operators."]);
    run_for(&mut g, 2.0);
    let coins = tokens(&g);
    assert_eq!(chat(&mut g, 4, "!givetokens 250").iter().filter(|(to, _)| *to == Target::Only(4)).map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["Added 250 Terra tokens."]);
    assert_eq!(tokens(&g), coins + 250);
    for bad in ["!givetokens 10001", "!givetokens lots", "!givetokens 0", "!givetokens -5"] {
        assert_eq!(told(&chat(&mut g, 4, bad), 4), ["Use: !givetokens <number from 1 to 10000>"], "{bad}");
    }
    assert_eq!(told(&chat(&mut g, 4, "!givetokens"), 4), ["Use: !givetokens <number from 1 to 10000>"]);
    // The host allows two token changes a second (on the clock of the process): the third in the same instant is told.
    let coins = tokens(&g);
    run_for(&mut g, 1.5);
    let answers: Vec<String> = (0..3).flat_map(|_| told(&chat(&mut g, 4, "!givetokens 1"), 4).into_iter().map(str::to_string).collect::<Vec<_>>()).collect();
    assert_eq!(answers, ["Added 1 Terra tokens.", "Added 1 Terra tokens.", "Token changes are limited to 2 a second: try again in a moment."]);
    assert_eq!(tokens(&g), coins + 2);

    // The books survive a hot update: nothing is paid twice.
    let (coins, irons) = (tokens(&g), count(&g, 1, "Iron"));
    let mut next = hot_update(&mut g, &dir);
    run_for(&mut next, 12.0);
    assert_eq!((tokens(&next), count(&next, 1, "Iron")), (coins, irons));
    assert!(talk(&mut next, 1, "!rewards")[0].starts_with("You have placed 10 buildings"));
    assert_eq!(plugin(&next, "quest-rewards").stats.faults, 0);
}

/// quest-rewards: with fewer free slots than `item_count` nothing is put in and the reward waits (the first run of this test found
/// the plugin giving what fitted and counting the whole reward as given).
#[test]
fn quest_rewards_give_only_what_fits_when_the_backpack_is_nearly_full() {
    let Some((mut g, _)) = rig("cb-quest-fits", &["quest-rewards"], &[("quest-rewards", "world_milestones", "999999999999")]) else { return };
    let (bag, _) = backpack(&g, 1);
    let free = g.inventories[&bag].size as usize - g.inventories[&bag].items.len();
    assert!(free > 0 && free < 10, "the test world's backpack has {free} free slots");
    let (iron, coins) = (count(&g, 1, "Iron"), tokens(&g));
    g.net.out.clear();
    build(&mut g, 1, "EnergyGenerator1", [600.0, 3.0, 500.0]).expect("built");
    run_for(&mut g, 12.0);
    // The tokens come at once; the ten Iron do not fit in five slots, so none is put in, and the player is told why.
    assert_eq!((count(&g, 1, "Iron"), tokens(&g)), (iron, coins + 100));
    let out = chat_out(&g);
    let heard = told(&out, 1);
    assert!(heard.iter().any(|t| t.contains("reward is waiting") && t.contains("10 free slots")), "{heard:?}");
    run_for(&mut g, 30.0);
    assert_eq!(count(&g, 1, "Iron"), iron, "and it is not made up in part");
    // Room is made: the whole reward is given, once.
    let others: Vec<i32> = g.inventories[&bag].items.iter().copied().filter(|id| g.data.groups[g.wos[id].group].id != "Iron").take(10).collect();
    for id in others {
        g.inventory_remove(bag, id);
    }
    run_for(&mut g, 12.0);
    assert_eq!(count(&g, 1, "Iron"), iron + 10);
    run_for(&mut g, 30.0);
    assert_eq!(count(&g, 1, "Iron"), iron + 10, "and not twice");
}

/// quest-rewards: the world milestone, when the terraformation passes a number of the list.
#[test]
fn quest_rewards_pay_a_world_milestone_once() {
    let Some((mut g, _)) = rig("cb-quest-world", &["quest-rewards"], &[("quest-rewards", "world_milestones", "5000,100000")]) else { return };
    // The test world has no terraformation yet: the world has grown to 7500, past the first milestone only.
    let hash = g.planet_hash();
    g.units.planet_mut(hash).unwrap().units[crate::units::KINDS.iter().position(|k| *k == crate::units::Kind::Terraformation).unwrap()].total = 7500.0;
    let coins = tokens(&g);
    let heard = run_chat(&mut g, 35.0);
    assert_eq!(said(&heard), ["World milestone: terraformation passed 5000! The colony receives 500 Terra tokens."], "{heard:?}");
    assert_eq!(tokens(&g), coins + 500);
    let heard = run_chat(&mut g, 100.0);
    assert!(said(&heard).is_empty() && tokens(&g) == coins + 500, "paid once: {heard:?}");
    let rewards = talk(&mut g, 1, "!rewards");
    assert_eq!(rewards[2], "World: terraformation 7500; the next milestone is 100000.");
}

// ---- claims.

/// claims: the one veto of the plugin API in the real game. A stranger's real build and deconstruct
/// requests inside a claim are refused (the world does not change, the player is told), outside they
/// are not; the owner, a trusted player and an operator pass; a hot update keeps the claims.
#[test]
fn claims_refuse_a_strangers_build_and_deconstruct_inside_a_claim() {
    let Some((mut g, dir)) = rig("cb-claims", &["claims"], &[]) else { return };
    for (client, name, at) in [(2, "Bo", [1500.0, 3.0, 1500.0]), (3, "Cy", [1400.0, 3.0, 1000.0]), (4, "Root", [900.0, 3.0, 900.0])] {
        join(&mut g, client, name);
        move_to(&mut g, client, at);
    }
    operator(&mut g, 4, 76561198000000004);
    move_to(&mut g, 1, [1000.0, 3.0, 1000.0]);
    assert_eq!(talk(&mut g, 1, "!claim"), ["Claimed radius 20 around (1000, 3, 1000). Only you and the players you !trust can build or take apart here."]);
    let refused_build = "This ground is claimed by Ann. You cannot build here.";
    let refused_apart = "This ground is claimed by Ann. You cannot take this apart here.";

    // Ann builds in her claim; Cy's request to build there is refused: no new object, no answer to the client,
    // and he is told once, however often he tries.
    let home = build(&mut g, 1, "Container1", [1005.0, 3.0, 1000.0]).expect("the owner builds");
    let objects = g.wos.len();
    g.net.out.clear();
    assert_eq!(build(&mut g, 3, "Container1", [1002.0, 3.0, 1003.0]), None);
    assert_eq!((g.wos.len(), told(&chat_out(&g), 3)), (objects, vec![refused_build]));
    g.net.out.clear();
    assert_eq!(build(&mut g, 3, "Container1", [1002.0, 3.0, 1003.0]), None);
    assert_eq!((g.wos.len(), chat_out(&g).len()), (objects, 0), "refused again, not told again");
    run_for(&mut g, 3.5);
    g.net.out.clear();
    assert_eq!(build(&mut g, 3, "Container1", [1002.0, 3.0, 1003.0]), None);
    assert_eq!(told(&chat_out(&g), 3), [refused_build], "three seconds on, he is told again");
    // Outside every claim he builds as he likes.
    assert!(build(&mut g, 3, "Container1", [1100.0, 3.0, 1000.0]).is_some());
    assert_eq!(g.wos.len(), objects + 1);

    // Taking Ann's building apart: refused for Cy, and the object stands; the owner may.
    run_for(&mut g, 3.5);
    g.net.out.clear();
    take_apart(&mut g, 3, home);
    assert!(g.net.objects.contains_key(&home) && g.wos.values().any(|w| w.net == Some(home)), "still standing");
    assert_eq!(told(&chat_out(&g), 3), [refused_apart]);
    assert!(sent(&g, "GroupNetworkBase.DeconstructClientRpc").is_empty());
    let own = build(&mut g, 1, "Container1", [1006.0, 3.0, 1004.0]).unwrap();
    take_apart(&mut g, 1, own);
    assert!(!g.net.objects.contains_key(&own), "the owner takes her own apart");

    // !trust: Bo (online) may; !untrust takes it back; an operator always may.
    assert_eq!(talk(&mut g, 1, "!trust Bo"), ["Bo can now build and take apart in your claims."]);
    take_apart(&mut g, 2, home);
    assert!(!g.net.objects.contains_key(&home), "Bo, trusted, takes it apart");
    assert!(build(&mut g, 2, "Container1", [1001.0, 3.0, 1001.0]).is_some(), "and builds");
    let home = build(&mut g, 1, "Container1", [1005.0, 3.0, 1000.0]).unwrap();
    assert_eq!(talk(&mut g, 1, "!claims"), ["Your claims (1 of 3): #1 at (1000, 3, 1000) radius 20, trusted: Bo;"]);
    assert_eq!(talk(&mut g, 1, "!untrust Bo"), ["Bo can no longer build or take apart in your claims."]);
    g.net.out.clear();
    take_apart(&mut g, 2, home);
    assert!(g.net.objects.contains_key(&home), "Bo is refused again");
    assert_eq!(told(&chat_out(&g), 2), [refused_apart]);
    // The host's own view: nothing it was handed was unreadable.
    assert!(plugin(&g, "claims").logs.last(100).iter().all(|l| !l.text.contains("letting it through")), "{:?}", plugin(&g, "claims").logs.last(100));

    // A hot update keeps the claims; the operator takes the building apart.
    let mut next = hot_update(&mut g, &dir);
    run_for(&mut next, 4.0);
    assert_eq!(build(&mut next, 3, "Container1", [1002.0, 3.0, 1003.0]), None, "the claim is still there");
    take_apart(&mut next, 4, home);
    assert!(!next.net.objects.contains_key(&home), "an operator may");
    let stats = &plugin(&next, "claims").stats;
    assert_eq!(stats.faults, 0);
    assert!(stats.vetoes >= 1);
}

/// What the claims README says an `rpc_before` event carries, read off a probe plugin that logs the payload of the two
/// RPCs it is subscribed to: the names, the arguments as text (the position is the second item of a build request),
/// and for a deconstruct the arguments are the animation's time.
#[test]
fn the_two_rpcs_of_claims_arrive_as_the_readme_says() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("cb-probe");
    let wat = r#"(module
          (import "pcds" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 1)
          (func (export "pcds_abi") (result i32) (i32.const 65536))
          (func (export "pcds_alloc") (param i32) (result i32) (i32.const 4096))
          (func (export "pcds_free") (param i32 i32))
          (func (export "pcds_event") (param $kind i32) (param $ptr i32) (param $len i32) (result i32)
            (call $log (i32.const 2) (local.get $ptr) (local.get $len)) (i32.const 0)))"#;
    let sub = "[events]\nsubscribe = [\"rpc_before\"]\nrpc = [\"GroupNetworkBase.DeconstructServerRpc\", \"WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc\"]";
    super::tests::install(&dir, "probe", wat, &["event.rpc"], sub, None);
    start(&mut g, &dir);
    join(&mut g, 3, "Cy");
    let net = build(&mut g, 3, "Container1", [1002.0, 3.0, 1003.0]).expect("built");
    let hash = hash(&g, "Container1");
    take_apart(&mut g, 3, net);
    let lines: Vec<String> = plugin(&g, "probe").logs.last(10).iter().map(|l| l.text.clone()).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    // The payload is the client (8 bytes), the RPC's name, the network object (8 bytes) and the arguments; the text parts are readable.
    assert!(lines[0].contains("WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc") && lines[0].ends_with(&format!("[{hash},[1002.0,3.0,1003.0],[0.0,0.0,0.0,1.0],true,false,true,false]")), "{lines:?}");
    assert!(lines[1].contains("GroupNetworkBase.DeconstructServerRpc") && lines[1].ends_with("[2.0]"), "{lines:?}");
    // The client and the network object are the right ones: the exact bytes the event is encoded as.
    let mut w = pcds_plugin_api::Writer::new();
    Event::RpcBefore { client: 3, rpc: "GroupNetworkBase.DeconstructServerRpc", object: net, args_json: "[2.0]" }.encode(&mut w);
    let readable: String = String::from_utf8_lossy(&w.0).chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    assert_eq!(lines[1], readable);
}

// ---- restart-notice.

/// The wall clock's UTC time of day as "HH:MM" for `minutes` minutes from the start of the current
/// minute, after waiting out the last seconds of a minute (the plugin reads the real clock).
fn restart_at(minutes: u64) -> String {
    let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    if clock().as_secs() % 60 >= 52 {
        std::thread::sleep(std::time::Duration::from_secs(9));
    }
    let minute_of_day = (clock().as_secs() / 60 + minutes) % 1440;
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

/// restart-notice: the warning before the restart and the restart line itself, at the real clock's
/// minutes but the game's seconds; `!restart`; the save and update announcements.
#[test]
fn restart_notice_warns_before_the_restart_and_tells_of_saves_and_updates() {
    // The restart is in under a minute: the line at the restart (it is within 90 s of the real clock).
    let at = restart_at(1);
    let Some((mut g, _)) = rig("cb-restart-now", &["restart-notice"], &[("restart-notice", "restart_at", &at), ("restart-notice", "warn_minutes", "none")]) else { return };
    let answer = talk(&mut g, 1, "!restart");
    assert_eq!(answer, [format!("Next restart in 1 minute (at {at} UTC).")]);
    let heard = run_chat(&mut g, 65.0);
    assert_eq!(said(&heard), ["The server restarts now"], "{heard:?}");

    // The restart is in two minutes and a warning one minute before it: the warning comes in the first minute.
    let at = restart_at(2);
    let settings = [("restart-notice", "restart_at", at.as_str()), ("restart-notice", "warn_minutes", "1"), ("restart-notice", "announce_saves", "true")];
    let Some((mut g, dir)) = rig("cb-restart-warn", &["restart-notice"], &settings) else { return };
    assert_eq!(talk(&mut g, 1, "!restart"), [format!("Next restart in 2 minutes (at {at} UTC).")]);
    // (The warning is due between 8 and 60 s after the start, the restart line at least 68 s after it.)
    let heard = run_chat(&mut g, 66.0);
    assert_eq!(said(&heard), ["Server restart in 1 minute"], "{heard:?}");
    // The restart line of this plugin is not tried: a timer counts game time, and the plugin does not say a line more than
    // 90 s before the real clock reads its time (it waits for the real time), so a test that runs the game ahead of the wall
    // clock cannot hear it unless it is due within 90 s (the block above).

    // A good save is told (not twice within 30 s of game time); a failed one always.
    g.net.out.clear();
    crate::events::world_saved(&mut g, true);
    frame(&mut g);
    assert_eq!(said(&chat_out(&g)), ["World saved."]);
    g.net.out.clear();
    crate::events::world_saved(&mut g, true);
    frame(&mut g);
    assert!(said(&chat_out(&g)).is_empty(), "within 30 s");
    g.net.out.clear();
    crate::events::world_saved(&mut g, false);
    frame(&mut g);
    assert_eq!(said(&chat_out(&g)), ["The world could not be saved, tell an admin."]);
    // A hot update is told, with the version.
    let mut next = hot_update(&mut g, &dir);
    frame(&mut next);
    assert_eq!(said(&chat_out(&next)), [format!("The server was updated to version {} without a restart.", env!("CARGO_PKG_VERSION"))]);

    // No restart time, and a restart time that is not a time.
    let Some((mut g, _)) = rig("cb-restart-none", &["restart-notice"], &[]) else { return };
    assert_eq!(talk(&mut g, 1, "!restart"), ["No restart is scheduled."]);
    let Some((mut g, _)) = rig("cb-restart-bad", &["restart-notice"], &[("restart-notice", "restart_at", "25:99")]) else { return };
    assert_eq!(talk(&mut g, 1, "!restart"), ["The restart time is not set up right, tell an admin."]);
}

// ---- chat-bridge.

/// chat-bridge: chat, joins and leaves become one batched request on the next tick; the webhook is
/// a placeholder the host fills in, never on a listing or in a log; the inbound half's server
/// message does not come back as a chat event.
#[test]
fn chat_bridge_batches_the_game_chat_into_one_request_and_keeps_the_secret() {
    let secret = "123456/secret-token-xyz";
    let Some((mut g, dir)) = rig_with("cb-bridge", &["chat-bridge"], &[("chat-bridge", "webhook", secret)], true) else { return };
    poll(&mut g, "watch", &["chat"], 0);
    let requests = |g: &Game| g.ext.plugins.host.as_ref().unwrap().offline.as_ref().unwrap().len();
    join(&mut g, 2, "Bo");
    chat(&mut g, 1, "hello @everyone <@123>");
    chat(&mut g, 2, "!home");
    assert_eq!(requests(&g), 0, "nothing is sent between ticks");
    run_for(&mut g, 1.5);
    let host = g.ext.plugins.host.as_mut().unwrap();
    let fetches = host.offline.as_mut().map(std::mem::take).unwrap();
    assert_eq!(fetches.len(), 1, "{fetches:?}");
    let f = &fetches[0];
    // The host put the admin's webhook into the address; the plugin only wrote `${config.webhook}`.
    assert_eq!((f.method.as_str(), f.url.as_str(), f.shown.as_str()), ("POST", "https://discord.com/api/webhooks/123456/secret-token-xyz", "https://discord.com"), "{f:?}");
    assert!(!String::from_utf8_lossy(&f.body).contains("secret-token-xyz"), "the plugin never held the secret");
    let body: Value = serde_json::from_slice(&f.body).unwrap();
    let content = body["content"].as_str().unwrap();
    eprintln!("bridge content: {content:?}");
    assert!(content.contains("**Bo** joined") && content.contains("**Ann**") && content.contains("hello @\u{200b}everyone"), "{content:?}");
    assert!(!content.contains("!home"), "commands are not conversation: {content:?}");
    assert_eq!(body["allowed_mentions"], json!({"parse": []}));
    // The secret is nowhere the host lists or logs.
    let listing = admin::request(&mut g, "GET", "/api/plugins", &json!({})).1;
    assert!(!listing.contains("secret-token-xyz") && !listing.contains("123456"), "the listing shows it");
    let p = plugin(&g, "chat-bridge");
    assert!(p.logs.last(200).iter().all(|l| !l.text.contains("secret-token-xyz")), "{:?}", p.logs.last(200));
    assert!(!serde_json::to_string(&g.ext.plugins.saved).unwrap().contains("secret-token-xyz"), "nor in the state");
    assert!(std::fs::read_to_string(dir.join("config.json")).unwrap().contains("secret-token-xyz"), "only the admin's file has it");
    // The request is answered; a later line goes in a new request after the wait (3 s of game time).
    answer_http(&mut g, f.ticket);
    g.disconnect(2);
    run_for(&mut g, 4.0);
    let fetches = g.ext.plugins.host.as_mut().unwrap().offline.as_mut().map(std::mem::take).unwrap();
    assert_eq!(fetches.len(), 1);
    assert!(String::from_utf8_lossy(&fetches[0].body).contains("**Bo** left"), "{:?}", String::from_utf8_lossy(&fetches[0].body));
    answer_http(&mut g, fetches[0].ticket);

    // The inbound half says things as the server (POST /api/chat ends in chathooks::say): the players hear it,
    // and neither the stream nor the chat hooks nor the bridge see a chat event.
    let before = poll(&mut g, "watch", &["chat"], 0)["events"].as_array().unwrap().len();
    let outbox = g.ext.chat.outbox.len();
    g.net.out.clear();
    crate::chathooks::say(&mut g, "[Discord] Zed: hi from the other side");
    run_for(&mut g, 4.0);
    // (run_for drains nothing: the message went out at the first frame)
    assert!(chat_out(&g).iter().any(|(to, t)| *to == Target::All && t == "[Discord] Zed: hi from the other side"));
    assert_eq!(poll(&mut g, "watch", &["chat"], 0)["events"].as_array().unwrap().len(), before, "no chat event for a server message");
    assert_eq!(g.ext.chat.outbox.len(), outbox, "nor in the chat hooks' queue");
    run_for(&mut g, 4.0);
    assert!(g.ext.plugins.host.as_ref().unwrap().offline.as_ref().unwrap().is_empty(), "so the bridge does not send it back");
    // (Still listening: a player's message in the same game goes out.)
    chat(&mut g, 1, "still here");
    run_for(&mut g, 4.0);
    let fetches = g.ext.plugins.host.as_ref().unwrap().offline.as_ref().unwrap();
    assert_eq!(fetches.len(), 1);
    let body = String::from_utf8_lossy(&fetches[0].body);
    assert!(body.contains("still here") && !body.contains("Zed"), "{body}");
    assert_eq!(plugin(&g, "chat-bridge").stats.faults, 0);
}

// ---- prometheus-exporter and the Python programs.

fn python(args: &[&str]) -> Option<std::process::Output> {
    match std::process::Command::new("python3").arg("-I").args(args).output() {
        Ok(out) => Some(out),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIPPED: no python3");
            None
        }
        Err(e) => panic!("python3: {e}"),
    }
}

/// prometheus-exporter: the stream and the plugin listing of a real game, rendered by `exporter.py --replay`.
#[test]
fn prometheus_exporter_renders_the_stream_and_the_listing_of_a_real_game() {
    let Some((mut g, dir)) = rig("cb-prom", &["hello-stats", "afk-guard"], &[]) else { return };
    let kinds = ["chat", "player_join", "player_leave", "object_placed", "object_destroyed", "world_saved", "rule_change"];
    poll(&mut g, "prometheus", &kinds, 0);
    join(&mut g, 2, "Bo");
    chat(&mut g, 1, "hello");
    chat(&mut g, 1, "!stats");
    chat(&mut g, 2, "hi Ann");
    let net = build(&mut g, 1, "Container1", [600.0, 3.0, 500.0]).unwrap();
    take_apart(&mut g, 1, net);
    crate::events::world_saved(&mut g, true);
    crate::events::world_saved(&mut g, false);
    g.disconnect(2);
    g.rules.speed.growth = 2.0;
    crate::events::rules_changed(&mut g);
    run_for(&mut g, 12.0);
    let events = poll(&mut g, "prometheus", &kinds, 0);
    let listing: Value = serde_json::from_str(&admin::request(&mut g, "GET", "/api/plugins", &json!({})).1).unwrap();
    let file = dir.join("bundle.json");
    std::fs::write(&file, json!({"events": events["events"], "status": {}, "plugins": listing}).to_string()).unwrap();
    let exporter = example("prometheus-exporter/exporter.py");
    let Some(out) = python(&[exporter.to_str().unwrap(), "--replay", file.to_str().unwrap()]) else { return };
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let line = |name: &str| text.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap_or_else(|| panic!("no {name} in\n{text}")).rsplit(' ').next().unwrap().to_string();
    assert_eq!(line("pcds_up"), "1");
    assert_eq!(line("pcds_chat_messages_total"), "3");
    assert_eq!((line("pcds_player_joins_total"), line("pcds_player_leaves_total")), ("1".to_string(), "1".to_string()));
    assert_eq!((line("pcds_objects_placed_total"), line("pcds_objects_destroyed_total"), line("pcds_rule_changes_total")), ("1".to_string(), "1".to_string(), "1".to_string()));
    assert!(text.contains("pcds_world_saves_total{result=\"ok\"} 1") && text.contains("pcds_world_saves_total{result=\"failed\"} 1"), "{text}");
    for id in ["hello-stats", "afk-guard"] {
        let calls = plugin(&g, id).stats.calls;
        assert!(calls > 0 && text.contains(&format!("pcds_plugin_calls_total{{plugin=\"{id}\"}} {calls}")), "{id}: {calls}\n{text}");
        assert!(text.contains(&format!("pcds_plugin_status{{plugin=\"{id}\",status=\"running\"}} 1")), "{text}");
        assert!(text.contains(&format!("pcds_plugin_faults_total{{plugin=\"{id}\"}} 0")), "{text}");
    }
}

/// The self-tests of the two Python programs (no network): part of the suite.
#[test]
fn the_python_programs_pass_their_self_tests() {
    for program in ["prometheus-exporter/exporter.py", "chat-bridge/bridge_in.py"] {
        let path = example(program);
        let Some(out) = python(&[path.to_str().unwrap(), "--self-test"]) else { return };
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("self-test ok"), "{program}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
}

// ---- All of them at once.

/// Every wasm example (and a data one) loaded together in one game: they take their turns in the order
/// of the folder, nothing faults, nothing asks for a capability it was not given.
#[test]
fn all_the_examples_run_together_without_faults_or_missing_capabilities() {
    let ids = ["afk-guard", "chat-bridge", "claims", "hello-c", "hello-stats", "playtime-board", "quest-rewards", "restart-notice", "scaling-rules"];
    let settings = [("chat-bridge", "webhook", "1/abc"), ("restart-notice", "restart_at", "04:00")];
    let Some((mut g, _)) = rig_with("cb-all", &ids, &settings, true) else { return };
    for (client, name) in [(2, "Bo"), (3, "Cy")] {
        join(&mut g, client, name);
    }
    operator(&mut g, 3, 76561198000000003);
    let started = std::time::Instant::now();
    let mut said_by_command = Vec::new();
    for command in ["!hello", "!roll", "!stats", "!uses", "!top", "!playtime", "!rewards", "!claims", "!restart", "!tier"] {
        said_by_command.push((command, talk(&mut g, 2, command)));
    }
    assert!(said_by_command.iter().all(|(c, answer)| !answer.is_empty() || *c == "!claims"), "{said_by_command:?}");
    move_to(&mut g, 2, [1000.0, 3.0, 1000.0]);
    assert_eq!(talk(&mut g, 2, "!claim").len(), 1);
    let nets: Vec<u64> = (0..3).filter_map(|i| build(&mut g, 2, "Container1", [1010.0 + 10.0 * i as f32, 3.0, 1000.0])).collect();
    assert_eq!(nets.len(), 3);
    take_apart(&mut g, 2, nets[0]);
    assert_eq!(build(&mut g, 1, "Container1", [1011.0, 3.0, 1002.0]), None, "Ann builds inside the claim Bo made, and the claims plugin refuses it");
    g.disconnect(3);
    run_for(&mut g, 60.0);
    let elapsed = started.elapsed();
    let frames_before = now(&g);
    for _ in 0..200 {
        frame(&mut g);
    }
    let two_hundred = started.elapsed() - elapsed;
    println!("200 ticks with {} plugins: {:?} ({:?} per tick); setup and 60 s: {:?}; game time {frames_before}", ids.len(), two_hundred, two_hundred / 200, elapsed);
    let host = g.ext.plugins.host.as_ref().unwrap();
    for p in &host.plugins {
        assert!(p.running(), "{}: {:?}", p.id, p.status);
        assert_eq!(p.stats.faults, 0, "{}: {:?}", p.id, p.stats.last_error);
        assert!(p.denied_caps.is_empty(), "{} used {:?} without it being granted", p.id, p.denied_caps);
        assert!(p.stats.calls > 0, "{} was never called", p.id);
    }
}
