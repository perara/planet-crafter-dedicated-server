//! What a server that runs plugins for months needs to hold: old modules keep loading when the ABI's minor
//! rises, plugins are called in a fixed order, a plugin set is reloaded a hundred times under a full server
//! without leaving anything behind, and a plugin switched off in the middle of a hot update loses nothing.

use serde_json::json;

use super::game_tests::{chat_out, example, install_example, start};
use super::tests::{fresh_dir, install, module, Mock};
use crate::game::Game;
use super::*;
use crate::testkit::{join, send_scene, world};

#[test]
fn a_module_built_for_any_older_minor_loads_and_runs_and_a_newer_one_is_refused_with_both_versions() {
    let dir = fresh_dir("minors");
    // 1.0, 1.1, 1.2 and 1.3: the version a module says it was built for is `major << 16 | minor`.
    for (id, abi) in [("v10", 65536), ("v11", 65537), ("v12", 65538), ("v13", 65539), ("v14", 65540), ("v20", 131072)] {
        install(&dir, id, &module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", "").replace("(i32.const 65536)", &format!("(i32.const {abi})")), &["event.chat", "chat.say"], "[events]\nsubscribe = [\"chat\"]", None);
    }
    let mut host = super::tests::boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &Event::Chat { client: 1, name: "Ann", text: "hi", player_id: 1 }, 0);
    for id in ["v10", "v11", "v12", "v13"] {
        let p = host.plugins.iter().find(|p| p.id == id).unwrap();
        assert!(p.running() && p.stats.faults == 0 && p.stats.calls == 1, "{id}: {:?}", p.status);
    }
    assert_eq!(world.applied.len(), 4, "each of the four said its line");
    for (id, says) in [("v14", "built for plugin ABI 1.4; this server speaks 1.3"), ("v20", "built for plugin ABI 2.0; this server speaks 1.3")] {
        let p = host.plugins.iter().find(|p| p.id == id).unwrap();
        assert!(matches!(&p.status, Status::Failed(why) if why.contains(says)), "{id}: {:?}", p.status);
    }
}

#[test]
fn plugins_are_called_in_the_order_of_their_ids_whatever_the_order_they_were_installed_in() {
    let dir = fresh_dir("order");
    // Each says a text of its own length, so the order of the applied lines names the order of the calls.
    for (id, len) in [("delta", 4), ("alpha", 1), ("echo", 5), ("bravo", 2), ("charlie", 3)] {
        install(&dir, id, &module(&format!("(drop (call $say (i32.const 0) (i32.const {len}))) (i32.const 0)"), ""), &["event.chat", "chat.say"], "[events]\nsubscribe = [\"chat\"]", None);
    }
    for round in 0..3 {
        let mut host = super::tests::boot(&dir);
        let mut world = Mock::default();
        host.run(&mut world, &Event::Chat { client: 1, name: "Ann", text: "hi", player_id: 1 }, 0);
        let order: Vec<&str> = world.applied.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, vec!["alpha", "bravo", "charlie", "delta", "echo"], "round {round}");
    }
}

#[test]
fn a_hundred_reloads_under_sixteen_players_leave_nothing_behind_and_keep_the_state() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("hundred");
    install_example(&dir, "hello-stats", None);
    install_example(&dir, "server-motd", None);
    install_example(&dir, "afk-guard", None);
    start(&mut g, &dir);
    for c in 2..=16 {
        join(&mut g, c, &format!("Bot{c}"));
    }
    let gauge = |g: &Game| g.ext.plugins.host.as_ref().unwrap().gauge.live();
    let steady = gauge(&g);
    assert_eq!(steady.0, 2, "two modules run (the third is a rules file)");
    let mut asked = 0u32;
    let t = std::time::Instant::now();
    for cycle in 0..100 {
        // Everyone talks, a plugin is switched off and on, a file changes under one of them, and the server goes on.
        for c in 1..=16u64 {
            g.sim_time = Some(g.sim_time.unwrap_or(0.0) + 0.25);
            g.tick();
            send_scene(&mut g, c, "ChatHandler.SendChatMessageServerRpc", &[json!(if c % 2 == 0 { "!uses" } else { "!motd" })]);
        }
        asked += 8;
        match cycle % 3 {
            0 => {
                disable(&dir, "hello-stats").unwrap();
                reload(&mut g).unwrap();
                assert!(g.ext.plugins.host.as_ref().unwrap().plugins.iter().find(|p| p.id == "hello-stats").is_some_and(|p| p.status == Status::Disabled));
                approve(&dir, "hello-stats", None).unwrap();
                reload(&mut g).unwrap();
            }
            1 => {
                // A new version of afk-guard whose digest differs (a custom section): approved, replaced, state handed over.
                let wasm = dir.join("afk-guard/plugin.wasm");
                let mut bytes = std::fs::read(&wasm).unwrap();
                bytes.extend_from_slice(&[0, 6, 4, b'n', b'o', b't', b'e', cycle as u8]);
                std::fs::write(&wasm, bytes).unwrap();
                approve(&dir, "afk-guard", None).unwrap();
                reload(&mut g).unwrap();
            }
            _ => {
                reload(&mut g).unwrap();
            }
        }
    }
    let host = g.ext.plugins.host.as_ref().unwrap();
    // The same instances; the pages a module grew to while it worked may differ by a few, but nothing piles up (a leak of a page a
    // cycle would be a hundred).
    let (instances, pages) = host.gauge.live();
    assert_eq!(instances, steady.0);
    assert!(pages <= steady.1 + 8, "{pages} pages after the hundred reloads, {} before", steady.1);
    for p in &host.plugins {
        assert!(p.running(), "{}: {:?}", p.id, p.status);
        assert_eq!(p.stats.faults, 0, "{}: {:?}", p.id, p.stats.last_error);
    }
    // hello-stats counted every !uses of the cycles it was on for, across the instances (the counter lives in its saved state).
    g.net.out.clear();
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    let said = chat_out(&g);
    assert!(said.iter().any(|(_, t)| t.starts_with("!stats was used 0 times, !uses ")), "{said:?}");
    println!("100 reloads with 16 players and {asked} commands each: {:?} ({:?} per cycle)", t.elapsed(), t.elapsed() / 100);
    let _ = example("hello-stats");
}

#[test]
fn a_plugin_switched_off_while_the_world_is_handed_over_keeps_its_state_on_disk() {
    let Some((mut g, _)) = world() else { return };
    let dir = fresh_dir("flip-off");
    install_example(&dir, "hello-stats", None);
    start(&mut g, &dir);
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!stats")]);
    send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    // The hand-over: the old bank saves the plugins' state into the snapshot...
    checkpoint(&mut g, Some(limits::FLIP_BUDGET));
    let snapshot = crate::handoff::snapshot(&mut g).unwrap();
    // ...the new bank read the folder before (the plugin was on) ...
    let prepared = prepare(&dir, "world");
    // ...and the admin switches it off while the world is handed over.
    disable(&dir, "hello-stats").unwrap();
    let dirs = crate::test_data().unwrap();
    let (n, d, s) = crate::tables(dirs);
    let scenes = crate::scene_tables(dirs, &n, &d);
    let mut next = crate::handoff::restore(n, d, s, &snapshot, scenes).unwrap();
    attach(&mut next, prepared, true);
    let host = next.ext.plugins.host.as_ref().unwrap();
    assert_eq!(host.plugins[0].status, Status::Disabled);
    assert_eq!(host.gauge.live(), (0, 0), "it was started with its state and unloaded straight after: nothing is left");
    // Its state went to its file, for when it is switched on again; and then it counts on from there.
    let blob = state::read(&state::path(&dir.join("hello-stats"), "world")).unwrap().expect("the state was written when it was unloaded");
    assert_eq!(blob.version, 2);
    approve(&dir, "hello-stats", None).unwrap();
    reload(&mut next).unwrap();
    next.net.out.clear();
    send_scene(&mut next, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!uses")]);
    assert!(chat_out(&next).iter().any(|(_, t)| t == "!stats was used 1 times, !uses 2 times on this world"), "{:?}", chat_out(&next));
}
