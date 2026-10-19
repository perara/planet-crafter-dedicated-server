//! The plugin host in a real game (game tables needed; the tests skip without them): events from
//! the game's own call sites, the example plugins, the snapshot of a hot update with plugin state in
//! it, a plugin that refuses an RPC, rule changes by a plugin, and the event stream.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::tests::{fresh_dir, install, module};
use super::*;
use crate::game::Game;
use crate::testkit::{join, send_scene, sent, world};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins").join(name)
}

/// Copies an example plugin into `dir` as `pcds plugin install` does, and approves it as given.
fn install_example(dir: &Path, name: &str, grant: Option<Vec<String>>) {
    let from = example(name);
    let m = read_folder(&from).manifest.unwrap_or_else(|| panic!("{name}: no manifest"));
    let to = dir.join(m.id());
    std::fs::create_dir_all(&to).unwrap();
    for file in [manifest::FILE, m.plugin.entry.as_str()] {
        std::fs::copy(from.join(file), to.join(file)).unwrap_or_else(|e| panic!("{name}/{file}: {e} (scripts/build-plugin-examples builds the module)"));
    }
    approve(dir, m.id(), grant).unwrap();
}

fn start(g: &mut Game, dir: &Path) {
    attach(g, prepare(dir, "world"));
}

/// What the server sent as chat messages since `g.net.out` was cleared: (to, text).
fn chat_out(g: &Game) -> Vec<(crate::net::Target, String)> {
    sent(g, "ChatHandler.ReceiveChatMessageClientRpc").into_iter().map(|s| (s.to, s.args[1].as_str().unwrap_or("").to_string())).collect()
}

#[test]
fn with_no_plugin_nobody_listens() {
    let Some((g, _)) = world() else { return };
    assert_eq!(g.ext.plugins.interest, 0);
    assert!(g.ext.plugins.host.is_none());
}

#[test]
fn the_example_plugins_run_in_the_game_and_their_state_goes_through_a_hot_update() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("game");
    install_example(&dir, "hello-stats", None);
    install_example(&dir, "welcome-rules", None);
    start(&mut g, &dir);
    let host = g.ext.plugins.host.as_ref().unwrap();
    assert!(host.plugins.iter().all(|p| p.running()), "{:?}", host.plugins.iter().map(|p| (&p.id, &p.status)).collect::<Vec<_>>());
    assert!(g.ext.plugins.interest & Kind::Chat.bit() != 0 && g.ext.plugins.interest & Kind::PlayerJoin.bit() != 0 && g.ext.plugins.interest & Kind::Tick.bit() != 0);

    // A player joins: the data plugin welcomes them (to everyone, and to them).
    join(&mut g, 2, "Bo");
    g.tick();
    let said = chat_out(&g);
    assert!(said.contains(&(crate::net::Target::All, "Welcome, Bo! 2 online.".into())), "{said:?}");
    assert!(said.contains(&(crate::net::Target::Only(2), "Type !rules for the rules.".into())), "{said:?}");

    // Chat commands: the wasm plugin answers !stats to the asker only; the data plugin !rules.
    g.net.out.clear();
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!stats")]);
    send_scene(&mut g, 2, "ChatHandler.SendChatMessageServerRpc", &[json!("!rules")]);
    let said = chat_out(&g);
    let stats = said.iter().find(|(to, t)| *to == crate::net::Target::Only(1) && t.contains("2 players")).unwrap_or_else(|| panic!("{said:?}"));
    assert!(stats.1.starts_with("Server on Prime: 2 players"), "{}", stats.1);
    assert!(said.iter().any(|(to, t)| *to == crate::net::Target::Only(2) && t.starts_with("1. Be kind.")), "{said:?}");
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    assert!(chat_out(&g).iter().any(|(to, t)| *to == crate::net::Target::Only(1) && t == "!stats was used 1 times, !uses 1 times on this world"), "{:?}", chat_out(&g));

    // The hot update: the state goes into the snapshot, the next bank prepares and starts its plugins from it.
    checkpoint(&mut g, Some(limits::FLIP_BUDGET));
    let snapshot = crate::handoff::snapshot(&g).unwrap();
    assert!(snapshot.contains("\"plugins\":{\"saved\":{\"states\":{\"hello-stats\":"), "the plugin's state is in the snapshot");
    let dirs = crate::test_data().unwrap();
    let (n, d, s) = crate::tables(dirs);
    let scenes = crate::scene_tables(dirs, &n, &d);
    let mut next = crate::handoff::restore(n, d, s, &snapshot, scenes).unwrap();
    crate::handoff::verify(&next, snapshot.as_bytes()).unwrap();
    assert!(next.ext.plugins.host.is_none() && next.ext.plugins.interest == 0, "a restored game has no host until the bank attaches one");
    start(&mut next, &dir);
    next.net.out.clear();
    send_scene(&mut next, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    assert!(chat_out(&next).iter().any(|(_, t)| t == "!stats was used 1 times, !uses 2 times on this world"), "the counters went on: {:?}", chat_out(&next));

    // An older snapshot, from before plugins, restores with no plugin state.
    let plain: Value = serde_json::from_str(snapshot.split_once('\n').unwrap().1).unwrap();
    assert!(plain["ext"]["plugins"]["saved"]["states"].is_object());
}

#[test]
fn a_data_plugin_changes_the_rules_with_the_players_online_and_the_stream_hears_of_it() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("rules");
    install_example(&dir, "welcome-rules", None);
    start(&mut g, &dir);
    // An integration starts listening.
    let (status, _) = admin::request(&mut g, "POST", "/api/plugins/events", &json!({"types": ["chat", "rule_change", "player_join"]}));
    assert_eq!(status, 200);
    assert!(g.ext.plugins.interest & Kind::RuleChange.bit() != 0);
    for client in 2..=6 {
        join(&mut g, client, &format!("P{client}"));
    }
    // Six players: from the next tick on, plants grow twice as fast (a rules change by a plugin,
    // through the same validated path as the admin API's).
    g.sim_time = Some(2.0);
    g.tick();
    assert_eq!(g.rules.speed.growth, 2.0);
    // Three of them leave: back to the game's speed.
    for client in 4..=6 {
        g.disconnect(client);
    }
    g.sim_time = Some(4.0);
    g.tick();
    assert_eq!(g.rules.speed.growth, 1.0);
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("hello all")]);
    let (_, answer) = admin::request(&mut g, "POST", "/api/plugins/events", &json!({"after": 0, "types": ["chat", "rule_change", "player_join"]}));
    let answer: Value = serde_json::from_str(&answer).unwrap();
    let types: Vec<&str> = answer["events"].as_array().unwrap().iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(types.iter().filter(|t| **t == "rule_change").count() == 2 && types.contains(&"chat") && types.iter().filter(|t| **t == "player_join").count() == 5, "{types:?}");
    let chat = answer["events"].as_array().unwrap().iter().find(|e| e["type"] == "chat").unwrap();
    assert_eq!((chat["name"].as_str(), chat["text"].as_str(), chat["client"].as_u64()), (Some("Ann"), Some("hello all"), Some(1)));
    let rule = answer["events"].as_array().unwrap().iter().find(|e| e["type"] == "rule_change").unwrap();
    assert_eq!(rule["changes"]["speed.growth"], 2.0);
    // The ring's ids and epoch are in the snapshot.
    let text = serde_json::to_string(&g.ext.plugins.saved.stream).unwrap();
    assert!(text.contains(&format!("\"epoch\":{}", g.ext.plugins.saved.stream.epoch)));
}

#[test]
fn a_plugin_that_may_refuse_an_rpc_stops_it_and_one_that_may_not_does_not() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("veto");
    let sub = "[events]\nsubscribe = [\"rpc_before\"]\nrpc = [\"ChatHandler.SendChatMessageServerRpc\"]";
    install(&dir, "no-chat", &module("(i32.const 1)", ""), &["event.rpc", "rpc.veto"], sub, None);
    start(&mut g, &dir);
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("hi")]);
    assert!(chat_out(&g).is_empty(), "the chat message never ran: {:?}", chat_out(&g));
    // Without the capability the same answer changes nothing.
    approve(&dir, "no-chat", Some(vec!["event.rpc".into()])).unwrap();
    reload(&mut g).unwrap();
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("hi")]);
    assert_eq!(chat_out(&g).len(), 1);
}

#[test]
fn reload_keeps_a_plugin_that_did_not_change_and_restarts_one_that_did_with_its_state() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("reload");
    install_example(&dir, "hello-stats", None);
    start(&mut g, &dir);
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!stats")]);
    g.net.out.clear();
    reload(&mut g).unwrap();
    assert_eq!(g.ext.plugins.host.as_ref().unwrap().plugins[0].stats.calls, 1, "the running plugin was kept, with its counters");
    // Changed on disk and approved again: replaced, started from the state the old one gave.
    let wasm = dir.join("hello-stats/plugin.wasm");
    let mut bytes = std::fs::read(&wasm).unwrap();
    // A custom section "note" with one byte: the module is as it was, to the digest another.
    bytes.extend_from_slice(&[0, 6, 4, b'n', b'o', b't', b'e', 0]);
    std::fs::write(&wasm, bytes).unwrap();
    reload(&mut g).unwrap();
    // The files wait for approval; the approved version, in memory, goes on and the listing says so.
    let p = &g.ext.plugins.host.as_ref().unwrap().plugins[0];
    assert!(p.running() && p.notice.as_deref().unwrap().contains("wait for approval"), "{:?} {:?}", p.status, p.notice);
    approve(&dir, "hello-stats", None).unwrap();
    reload(&mut g).unwrap();
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    assert!(chat_out(&g).iter().any(|(_, t)| t == "!stats was used 1 times, !uses 1 times on this world"), "{:?}", chat_out(&g));
}

#[test]
fn the_admin_api_lists_enables_and_disables_plugins() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("api");
    // Installed, not approved.
    let from = example("welcome-rules");
    let to = dir.join("welcome-rules");
    std::fs::create_dir_all(&to).unwrap();
    for f in ["plugin.toml", "rules.toml"] {
        std::fs::copy(from.join(f), to.join(f)).unwrap();
    }
    start(&mut g, &dir);
    let (status, body) = admin::request(&mut g, "GET", "/api/plugins", &json!({}));
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((status, list["plugins"][0]["status"].as_str()), (200, Some("pending")));
    assert!(list["capabilities"].as_array().unwrap().len() >= 12 && list["abi"]["major"] == 1);
    let (status, body) = admin::request(&mut g, "POST", "/api/plugins/enable", &json!({"id": "welcome-rules", "grant": ["event.chat", "chat.say"]}));
    let enabled: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((status, enabled["plugin"]["status"].as_str()), (200, Some("running")), "{body}");
    assert_eq!(enabled["plugin"]["granted"], json!(["event.chat", "chat.say"]));
    // Asking for more than the manifest does is refused; so is an unknown plugin.
    let (status, _) = admin::request(&mut g, "POST", "/api/plugins/enable", &json!({"id": "welcome-rules", "grant": ["player.kick"]}));
    assert_eq!(status, 400);
    assert_eq!(admin::request(&mut g, "POST", "/api/plugins/enable", &json!({"id": "nope"})).0, 400);
    assert_eq!(admin::request(&mut g, "POST", "/api/plugins/enable", &json!({})).0, 400);
    let (_, body) = admin::request(&mut g, "POST", "/api/plugins/disable", &json!({"id": "welcome-rules"}));
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["plugin"]["status"], "disabled");
    assert_eq!(g.ext.plugins.interest, 0, "disabled: nobody listens again");
    assert_eq!(admin::request(&mut g, "GET", "/api/plugins/nope", &json!({})).0, 404);
    let status = crate::plugins::admin::summary(&g);
    assert_eq!(status["loaded"], 1);
}

fn ask(g: &mut Game, client: u64, text: &str) -> Vec<String> {
    g.net.out.clear();
    // The game's own chat rate limit (a burst of 10, then one a second) is not what is tested here.
    // Nor is the RPC budget of a tick: two seconds of game time and a tick between asks.
    g.sim_time = Some(g.sim_time.unwrap_or(0.0) + 2.0);
    g.tick();
    send_scene(g, client, "ChatHandler.SendChatMessageServerRpc", &[json!(text)]);
    chat_out(g).into_iter().filter(|(to, _)| *to == crate::net::Target::Only(client)).map(|(_, t)| t).collect()
}

#[test]
fn plugins_appear_change_and_go_while_the_game_runs_without_being_asked() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("live");
    // The server runs with an empty plugins folder: no host, nobody listening.
    start(&mut g, &dir);
    assert!(g.ext.plugins.host.is_none() && g.ext.plugins.interest == 0);
    poll_now(&mut g);
    assert!(ask(&mut g, 1, "!uses").is_empty());

    // A plugin is dropped into the folder, and approved (as `pcds plugin enable` does with the server
    // not asked): the scan sees it, two looks in a row, and it starts.
    let from = example("hello-stats");
    let to = dir.join("hello-stats");
    std::fs::create_dir_all(&to).unwrap();
    for f in ["plugin.toml", "plugin.wasm"] {
        std::fs::copy(from.join(f), to.join(f)).unwrap();
    }
    poll_now(&mut g);
    assert!(g.ext.plugins.host.is_none(), "one look is not enough: the files may still be arriving");
    poll_now(&mut g);
    let status = || g.ext.plugins.host.as_ref().map(|h| h.plugins[0].status.name());
    assert_eq!(status(), Some("pending"), "discovered, waiting for approval");
    assert!(ask(&mut g, 1, "!uses").is_empty(), "and doing nothing");
    approve(&dir, "hello-stats", None).unwrap();
    poll_now(&mut g);
    poll_now(&mut g);
    assert_eq!(g.ext.plugins.host.as_ref().unwrap().plugins[0].status, Status::Running);
    assert_eq!(ask(&mut g, 1, "!uses"), vec!["!stats was used 0 times, !uses 1 times on this world"]);

    // Idle: the scan does nothing (no rescan, the same instance).
    let calls = g.ext.plugins.host.as_ref().unwrap().plugins[0].stats.calls;
    for _ in 0..5 {
        poll_now(&mut g);
    }
    assert_eq!(g.ext.plugins.host.as_ref().unwrap().plugins[0].stats.calls, calls);

    // Switched off while players chat: the command is gone at once, the state kept.
    disable(&dir, "hello-stats").unwrap();
    for _ in 0..2 {
        assert!(g.ext.plugins.host.as_ref().is_some_and(|h| h.plugins[0].running()), "until the second look");
        ask(&mut g, 1, "!stats");
        poll_now(&mut g);
    }
    assert!(ask(&mut g, 1, "!uses").is_empty());
    assert_eq!(g.ext.plugins.interest, 0);
    assert_eq!(g.ext.plugins.host.as_ref().unwrap().gauge.live(), (0, 0));
    assert!(state::path(&to, "world").is_file());

    // And on again, then 20 quick toggles with chat between: it never answers wrongly, nothing piles up.
    for i in 0..20 {
        approve(&dir, "hello-stats", None).unwrap();
        reload(&mut g).unwrap();
        assert!(!ask(&mut g, 1, "!stats").is_empty(), "cycle {i}");
        disable(&dir, "hello-stats").unwrap();
        reload(&mut g).unwrap();
        let answer = ask(&mut g, 1, "!stats");
        assert!(answer.is_empty(), "cycle {i}: {answer:?} {:?}", g.ext.plugins.host.as_ref().map(|h| h.plugins.iter().map(|p| (p.id.clone(), p.status.clone(), p.rt.is_some())).collect::<Vec<_>>()));
    }
    assert_eq!(g.ext.plugins.host.as_ref().unwrap().gauge.live(), (0, 0));

    // The folder removed: the plugin and its host are gone, nothing is written back.
    approve(&dir, "hello-stats", None).unwrap();
    reload(&mut g).unwrap();
    std::fs::remove_dir_all(&to).unwrap();
    poll_now(&mut g);
    poll_now(&mut g);
    assert!(g.ext.plugins.host.is_none() && g.ext.plugins.interest == 0 && !to.exists());
    assert!(ask(&mut g, 1, "!uses").is_empty());
}

#[test]
fn the_plugin_set_after_a_flip_is_the_folders_at_that_moment() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("flipset");
    install_example(&dir, "hello-stats", None);
    install_example(&dir, "welcome-rules", None);
    // The next bank has read and compiled the folder (before its hello)...
    let prepared = prepare(&dir, "world");
    // ...and while the world is being handed over the admin switches one off.
    disable(&dir, "welcome-rules").unwrap();
    attach(&mut g, prepared);
    let host = g.ext.plugins.host.as_ref().unwrap();
    let by = |id: &str| host.plugins.iter().find(|p| p.id == id).unwrap().status.name();
    assert_eq!((by("hello-stats"), by("welcome-rules")), ("running", "disabled"), "the folder's set, at once, not the compile's");
    assert_eq!(host.gauge.live().0, 1);
}

#[test]
fn stream_consumers_register_through_the_api_and_leave_cleanly() {
    let Some((mut g, _)) = world() else { return };
    start(&mut g, &fresh_dir("consumers"));
    let poll = |g: &mut Game, body: Value| admin::request(g, "POST", "/api/plugins/events", &body);
    assert_eq!(poll(&mut g, json!({"consumer": "bad name!"})).0, 400);
    assert_eq!(poll(&mut g, json!({"consumer": "relay", "types": ["chat"]})).0, 200);
    assert_eq!(poll(&mut g, json!({"consumer": "metrics", "types": ["player_join"]})).0, 200);
    assert!(g.ext.plugins.interest & Kind::Chat.bit() != 0 && g.ext.plugins.interest & Kind::PlayerJoin.bit() != 0);
    let list: Value = serde_json::from_str(&admin::request(&mut g, "GET", "/api/plugins", &json!({})).1).unwrap();
    let names: Vec<&str> = list["stream"]["consumers"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names, vec!["metrics", "relay"]);
    // One leaves: its kinds are no longer recorded; the other goes on.
    let (status, body) = poll(&mut g, json!({"consumer": "relay", "unregister": true}));
    assert_eq!((status, serde_json::from_str::<Value>(&body).unwrap()["was"].as_bool()), (200, Some(true)));
    assert!(g.ext.plugins.interest & Kind::Chat.bit() == 0 && g.ext.plugins.interest & Kind::PlayerJoin.bit() != 0);
    poll(&mut g, json!({"consumer": "metrics", "unregister": true}));
    assert_eq!(g.ext.plugins.interest, 0);
}
