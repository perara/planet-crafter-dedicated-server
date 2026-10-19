//! The host against plugins that misbehave, with no game and no game tables: the hostile plugins
//! are written as WebAssembly text, the world is a mock. What each must show is the same: the call
//! ends within its budget, the offence is counted and logged, the host and the other plugins carry
//! on, and the world is changed only by what the capabilities allow.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pcds_plugin_api::query::{PlayerInfo, WorldInfo};
use pcds_plugin_api::{Event, Kind, Verdict};

use super::ctx::{Action, World};
use super::state::Saved;
use super::*;

#[derive(Default)]
pub(super) struct Mock {
    pub applied: Vec<(String, Action)>,
    pub players: u32,
}

impl World for Mock {
    fn info(&self) -> WorldInfo {
        WorldInfo { planet: "Prime".into(), players: self.players, game_ms: 61_000, ..Default::default() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        (0..self.players).map(|i| PlayerInfo { client: i as u64 + 1, name: format!("P{i}"), ..Default::default() }).collect()
    }
    fn rules(&self) -> String {
        "{}".into()
    }
}

impl Sink for Mock {
    fn apply(&mut self, plugin: &str, queued: Vec<Action>) -> Applied {
        let n = queued.len() as u32;
        self.applied.extend(queued.into_iter().map(|a| (plugin.to_string(), a)));
        Applied { done: n, refused: 0 }
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
}

pub(super) fn fresh_dir(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!("pcds-plugins-{}-{name}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A module with the exports the contract requires; `body` is what `pcds_event` does, `extra`
/// more functions or imports. `pcds_alloc` hands out one fixed buffer.
pub(super) fn module(body: &str, extra: &str) -> String {
    format!(
        r#"(module
          (import "pcds" "query" (func $query (param i32 i32 i32 i32 i32) (result i32)))
          (import "pcds" "say" (func $say (param i32 i32) (result i32)))
          (import "pcds" "kick" (func $kick (param i64 i32 i32) (result i32)))
          (import "pcds" "rules_patch" (func $patch (param i32 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "hello from a plugin")
          (func (export "pcds_abi") (result i32) (i32.const 65536))
          (func (export "pcds_alloc") (param i32) (result i32) (i32.const 4096))
          (func (export "pcds_free") (param i32 i32))
          (func (export "pcds_event") (param $kind i32) (param $ptr i32) (param $len i32) (result i32) {body})
          {extra})"#
    )
}

/// Installs a plugin in `dir` and approves it with `grant` (None: all it asks for).
pub(super) fn install(dir: &Path, id: &str, wat: &str, caps: &[&str], extra_manifest: &str, grant: Option<&[&str]>) {
    let folder = dir.join(id);
    std::fs::create_dir_all(&folder).unwrap();
    let caps_list = caps.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
    std::fs::write(
        folder.join("plugin.toml"),
        format!("[plugin]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\nentry = \"plugin.wasm\"\nabi = 1\ncapabilities = [{caps_list}]\n{extra_manifest}\n"),
    )
    .unwrap();
    std::fs::write(folder.join("plugin.wasm"), wat::parse_str(wat).unwrap_or_else(|e| panic!("{id}: {e}"))).unwrap();
    approve(dir, id, grant.map(|g| g.iter().map(|s| s.to_string()).collect())).unwrap();
}

pub(super) fn boot(dir: &Path) -> Host {
    let mut host = prepare(dir, "world");
    host.start_all(&Saved::default());
    host
}

fn chat(text: &str) -> Event<'_> {
    Event::Chat { client: 1, name: "Ann", text }
}

fn plugin<'a>(host: &'a Host, id: &str) -> &'a Loaded {
    host.plugins.iter().find(|p| p.id == id).unwrap()
}

const CHAT_SAY: [&str; 2] = ["event.chat", "chat.say"];
const SUBSCRIBE_CHAT: &str = "[events]\nsubscribe = [\"chat\"]";

#[test]
fn a_plugin_that_loops_forever_is_stopped_by_its_fuel_and_the_host_goes_on() {
    let dir = fresh_dir("loop");
    install(&dir, "looper", &module("(loop $l (br $l)) (i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "good", &module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    let t = Instant::now();
    host.run(&mut world, &chat("hi"), 0);
    // 1M fuel is a third of a millisecond: the loop is cut well within a few.
    assert!(t.elapsed() < Duration::from_millis(250), "{:?}", t.elapsed());
    let l = plugin(&host, "looper");
    assert_eq!((l.stats.faults, l.stats.restarts), (1, 1));
    assert!(l.stats.last_error.as_deref().unwrap().contains("out of fuel"), "{:?}", l.stats.last_error);
    assert!(l.running(), "one fault does not switch a plugin off");
    // The other plugin was called in the same round and said its piece.
    assert_eq!(world.applied, vec![("good".to_string(), Action::Say("hello".into()))]);
}

#[test]
fn five_faults_in_a_minute_switch_the_plugin_off_until_it_is_reloaded() {
    let dir = fresh_dir("breaker");
    install(&dir, "trapper", &module("unreachable", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    for i in 0..limits::MAX_FAULTS {
        assert!(plugin(&host, "trapper").running(), "still on before fault {i}");
        host.run(&mut world, &chat("x"), 0);
    }
    let p = plugin(&host, "trapper");
    assert!(!p.running() && matches!(&p.status, Status::Failed(why) if why.contains("switched off after 5 faults") && why.contains("unreachable")), "{:?}", p.status);
    assert_eq!(p.stats.faults, 5);
    // Off: it is not called any more.
    host.run(&mut world, &chat("x"), 0);
    assert_eq!(plugin(&host, "trapper").stats.calls, 5);
    host.recompute();
    assert_eq!(host.interest, 0, "and nothing wants chat events any more");
}

#[test]
fn memory_is_limited_and_a_module_that_asks_for_too_much_does_not_start() {
    let dir = fresh_dir("memory");
    // At run time: growing past the limit fails (returns -1), and the module can only tell.
    install(&dir, "grower", &module("(if (i32.eq (memory.grow (i32.const 5000)) (i32.const -1)) (then (return (i32.const 7)))) (i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    // At the start: a memory bigger than the limit is refused.
    let big = module("(i32.const 0)", "").replace("(memory (export \"memory\") 1)", "(memory (export \"memory\") 5000)");
    install(&dir, "big", &big, &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    host.run(&mut Mock::default(), &chat("x"), 0);
    let g = plugin(&host, "grower");
    assert!(g.stats.last_error.as_deref().unwrap().contains("returned 7"), "the growth failed: {:?}", g.stats.last_error);
    assert!(matches!(&plugin(&host, "big").status, Status::Failed(why) if why.contains("cannot start")), "{:?}", plugin(&host, "big").status);
}

#[test]
fn nonsense_answers_and_pointers_are_faults_not_crashes() {
    let dir = fresh_dir("nonsense");
    install(&dir, "returns-five", &module("(i32.const 5)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "reports-error", &module("(i32.const -9)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "bad-pointer", &module("(drop (call $say (i32.const 65530) (i32.const 100))) (i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "no-memory-for-us", &module("(i32.const 0)", "").replace("(i32.const 4096))", "(i32.const -1))"), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "huge-state", &module("(i32.const 0)", "(func (export \"pcds_save\") (result i64) (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 4000000000)))"), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "state-outside", &module("(i32.const 0)", "(func (export \"pcds_save\") (result i64) (i64.or (i64.shl (i64.const 70000) (i64.const 32)) (i64.const 100)))"), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &chat("x"), 0);
    fn err(host: &Host, id: &str) -> String {
        plugin(host, id).stats.last_error.clone().unwrap_or_default()
    }
    let e = |id: &str| err(&host, id);
    assert!(e("returns-five").contains("returned 5"), "{}", e("returns-five"));
    assert!(e("reports-error").contains("reported error -9"), "{}", e("reports-error"));
    assert!(e("bad-pointer").contains("pointer outside its memory"), "{}", e("bad-pointer"));
    assert!(e("no-memory-for-us").contains("outside the memory"), "{}", e("no-memory-for-us"));
    assert!(world.applied.is_empty(), "nothing a faulty call asked for is done");
    let mut saved = Saved::default();
    host.checkpoint(&mut saved, None);
    assert!(err(&host, "huge-state").contains("at most"), "{}", err(&host, "huge-state"));
    assert!(err(&host, "state-outside").contains("outside the memory"), "{}", err(&host, "state-outside"));
    assert!(saved.states.is_empty());
}

#[test]
fn a_module_with_imports_the_server_does_not_have_is_refused_by_name() {
    let dir = fresh_dir("imports");
    let wasi = module("(i32.const 0)", "").replacen("(module", "(module (import \"wasi_snapshot_preview1\" \"fd_write\" (func (param i32 i32 i32 i32) (result i32)))", 1);
    install(&dir, "wasi", &wasi, &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "garbage", "(module)", &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let host = boot(&dir);
    assert!(matches!(&plugin(&host, "wasi").status, Status::Invalid(why) if why.contains("wasi_snapshot_preview1.fd_write")), "{:?}", plugin(&host, "wasi").status);
    assert!(matches!(&plugin(&host, "garbage").status, Status::Invalid(why) if why.contains("does not export")), "{:?}", plugin(&host, "garbage").status);
    // Bytes that are no module at all.
    std::fs::write(dir.join("garbage/plugin.wasm"), b"MZ this is not wasm").unwrap();
    approve(&dir, "garbage", None).unwrap();
    assert!(matches!(&boot(&dir).plugins[0].status, Status::Invalid(why) if why.contains("not a valid WebAssembly")));
}

#[test]
fn a_module_built_for_another_abi_does_not_run() {
    let dir = fresh_dir("abi");
    install(&dir, "future", &module("(i32.const 0)", "").replace("(i32.const 65536)", "(i32.const 131072)"), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    install(&dir, "newer-minor", &module("(i32.const 0)", "").replace("(i32.const 65536)", "(i32.const 65537)"), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let host = boot(&dir);
    for id in ["future", "newer-minor"] {
        assert!(matches!(&plugin(&host, id).status, Status::Failed(why) if why.contains("plugin ABI")), "{id}: {:?}", plugin(&host, id).status);
    }
}

#[test]
fn capabilities_are_enforced_at_the_call_and_at_the_events() {
    let dir = fresh_dir("caps");
    // Asks for kick and rules.write, is granted neither; also asks for chat but is granted only that.
    let wat = module("(drop (call $kick (i64.const 1) (i32.const 0) (i32.const 3))) (drop (call $patch (i32.const 0) (i32.const 2))) (drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", "");
    install(&dir, "greedy", &wat, &["event.chat", "chat.say", "player.kick", "rules.write"], SUBSCRIBE_CHAT, Some(&["event.chat", "chat.say"]));
    // Granted the say but not the event: never called.
    install(&dir, "deaf", &module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, Some(&["chat.say"]));
    let mut host = boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &chat("x"), 0);
    assert_eq!(world.applied, vec![("greedy".to_string(), Action::Say("hello".into()))]);
    assert_eq!(plugin(&host, "greedy").stats.denied, 2);
    assert_eq!(plugin(&host, "deaf").stats.calls, 0);
    assert!(plugin(&host, "deaf").warnings.iter().any(|w| w.contains("event.chat is not granted")));
    // The admin can only grant what the manifest asks for.
    assert!(approve(&dir, "deaf", Some(vec!["player.kick".into()])).unwrap_err().contains("does not ask for player.kick"));
}

#[test]
fn an_installed_folder_is_named_for_the_plugin_id_and_a_source_folder_may_be_named_as_it_likes() {
    let dir = fresh_dir("name");
    install(&dir, "right", &module("(i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    // As a source folder it is fine, whatever it is called (pcds plugin check .).
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::copy(dir.join("right/plugin.toml"), dir.join("src/plugin.toml")).unwrap();
    std::fs::copy(dir.join("right/plugin.wasm"), dir.join("src/plugin.wasm")).unwrap();
    // (The check has its say about the imports of this test module, which the manifest does not ask for; not about the name.)
    let report = check::check_folder(&dir.join("src"), None);
    assert!(report.errors.iter().all(|e| !e.contains("folder")) && report.errors.iter().any(|e| e.contains("needs world.read")), "{:?}", report.errors);
    assert_eq!(read_folder(&dir.join("src")).id, "right");
    std::fs::remove_dir_all(dir.join("src")).unwrap();
    // Installed under another name it is not run, and says why.
    std::fs::rename(dir.join("right"), dir.join("wrong")).unwrap();
    let host = boot(&dir);
    assert!(matches!(&host.plugins[0].status, Status::Invalid(why) if why.contains("the folder is wrong but the manifest's id is right")), "{:?}", host.plugins[0].status);
    assert!(host.plugins[0].id == "wrong" && host.interest == 0);
}

#[test]
fn a_changed_file_waits_for_the_admin_again() {
    let dir = fresh_dir("digest");
    install(&dir, "p", &module("(i32.const 0)", ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    assert!(boot(&dir).plugins[0].running());
    // The module is swapped after the approval.
    std::fs::write(dir.join("p/plugin.wasm"), wat::parse_str(module("(i32.const 1)", "")).unwrap()).unwrap();
    let host = boot(&dir);
    assert_eq!(host.plugins[0].status, Status::Changed);
    assert!(host.plugins[0].rt.is_none() && host.interest == 0, "and none of it runs");
    // Never approved, disabled, and an approvals file that does not read.
    approve(&dir, "p", None).unwrap();
    disable(&dir, "p").unwrap();
    assert_eq!(boot(&dir).plugins[0].status, Status::Disabled);
    std::fs::write(dir.join("approved.json"), "{ nope").unwrap();
    let host = boot(&dir);
    assert!(host.approvals_error.is_some() && host.plugins[0].status == Status::Pending, "an unreadable list allows nothing");
}

#[test]
fn only_a_plugin_that_may_veto_can_veto_an_rpc() {
    let dir = fresh_dir("veto");
    let veto = module("(i32.const 1)", "");
    let sub = "[events]\nsubscribe = [\"rpc_before\"]\nrpc = [\"Inventories.Move\"]";
    install(&dir, "may", &veto, &["event.rpc", "rpc.veto"], sub, None);
    install(&dir, "a-may-not", &veto, &["event.rpc"], sub, None);
    let event = Event::RpcBefore { client: 1, rpc: "Inventories.Move", object: 3, args_json: "[]" };
    let other = Event::RpcBefore { client: 1, rpc: "Other.Rpc", object: 3, args_json: "[]" };
    let mut host = boot(&dir);
    let mut world = Mock::default();
    assert_eq!(host.run(&mut world, &event, 0), Verdict::Veto);
    assert_eq!(host.run(&mut world, &other, 0), Verdict::Continue, "an RPC the manifest does not name is not even shown");
    assert_eq!((plugin(&host, "may").stats.vetoes, plugin(&host, "a-may-not").stats.vetoes, plugin(&host, "a-may-not").stats.denied), (1, 0, 1));
}

#[test]
fn actions_are_bounded_per_call_and_applied_after_it() {
    let dir = fresh_dir("flood");
    let mut body = String::new();
    for _ in 0..100 {
        body += "(drop (call $say (i32.const 0) (i32.const 5)))";
    }
    install(&dir, "flood", &module(&format!("{body} (i32.const 0)"), ""), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &chat("x"), 0);
    assert_eq!(world.applied.len(), limits::ACTIONS_PER_CALL);
    assert_eq!(plugin(&host, "flood").stats.refused as usize, 100 - limits::ACTIONS_PER_CALL);
}

#[test]
fn a_crowd_of_plugins_cannot_stretch_a_tick_and_takes_turns() {
    let dir = fresh_dir("crowd");
    // Each burns about 0.3 ms: 1M iterations of 3 fuel is 3M; set its fuel to the maximum and loop 300k times.
    let busy = module("(local $i i32) (loop $l (local.set $i (i32.add (local.get $i) (i32.const 1))) (br_if $l (i32.lt_u (local.get $i) (i32.const 200000)))) (i32.const 0)", "");
    for i in 0..12 {
        install(&dir, &format!("busy{i:02}"), &busy, &["event.tick"], "[events]\nsubscribe = [\"tick\"]\n[budget]\ntick_every_ms = 5\nfuel_per_event = 20000000", None);
    }
    let mut host = boot(&dir);
    let mut world = Mock::default();
    let tick = |ms: i64| Event::Tick { game_ms: ms, tick: 0, players: 0, since_ms: 0 };
    let t = Instant::now();
    host.run(&mut world, &tick(1000), 1000);
    let spent = t.elapsed();
    let calls: u64 = host.plugins.iter().map(|p| p.stats.calls).sum();
    let shed: u64 = host.plugins.iter().map(|p| p.stats.shed).sum();
    assert!(calls >= 1 && shed >= 1 && calls < 12, "the budget cut the round: {calls} called, {shed} shed");
    // The budget plus the call that was running when it ran out.
    assert!(spent < limits::TICK_BUDGET + Duration::from_millis(40), "{spent:?}");
    // The next tick starts with the plugin that was left out.
    host.run(&mut world, &tick(1010), 1010);
    assert!(host.plugins.iter().filter(|p| p.stats.calls > 0).count() as u64 > calls);
}

#[test]
fn commands_go_to_their_owner_only_and_a_second_claim_is_told() {
    let dir = fresh_dir("commands");
    let say = module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", "");
    install(&dir, "aaa", &say, &CHAT_SAY, "[[command]]\nname = \"stats\"", None);
    install(&dir, "bbb", &say, &CHAT_SAY, "[[command]]\nname = \"STATS\"\n[[command]]\nname = \"other\"", None);
    let mut host = boot(&dir);
    assert!(plugin(&host, "bbb").warnings.iter().any(|w| w.contains("belongs to aaa")));
    let mut world = Mock::default();
    let command = |c| Event::Command { client: 1, name: "Ann", command: c, args: vec![], text: "!x" };
    host.run(&mut world, &command("stats"), 0);
    host.run(&mut world, &command("Other"), 0);
    host.run(&mut world, &command("nope"), 0);
    assert_eq!(world.applied.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["aaa", "bbb"]);
    assert!(host.interest & Kind::Command.bit() != 0 && host.interest & Kind::Chat.bit() != 0);
}

/// A counter: every event adds one and says it as a digit; its state is the count.
fn counter() -> String {
    module(
        "(global.set $count (i32.add (global.get $count) (i32.const 1))) (i32.store8 (i32.const 0) (i32.add (i32.const 48) (global.get $count))) (drop (call $say (i32.const 0) (i32.const 1))) (i32.const 0)",
        r#"(global $count (mut i32) (i32.const 0))
           (func (export "pcds_save") (result i64) (i32.store (i32.const 2048) (global.get $count)) (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const 4)))
           (func (export "pcds_state_version") (result i32) (i32.const 3))
           (func (export "pcds_load") (param i32 i32 i32) (result i32)
             (if (i32.ne (local.get 1) (i32.const 0))
               (then (if (i32.ne (local.get 2) (i32.const 3)) (then unreachable)) (global.set $count (i32.load (local.get 0)))))
             (i32.const 0))"#,
    )
}

fn said(world: &Mock) -> Vec<String> {
    world.applied.iter().map(|(_, a)| if let Action::Say(t) = a { t.clone() } else { String::new() }).collect()
}

#[test]
fn state_survives_a_hot_update_a_crash_and_a_restart() {
    let dir = fresh_dir("state");
    install(&dir, "counter", &counter(), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &chat("a"), 0);
    host.run(&mut world, &chat("b"), 0);
    assert_eq!(said(&world), vec!["1", "2"]);

    // The old bank's checkpoint goes into the snapshot as JSON; the new bank prepares its plugins
    // meanwhile and starts them from the snapshot's part.
    let mut saved = Saved::default();
    host.checkpoint(&mut saved, Some(limits::FLIP_BUDGET));
    let snapshot = serde_json::to_string(&saved).unwrap();
    let restored: Saved = serde_json::from_str(&snapshot).unwrap();
    let mut next = prepare(&dir, "world");
    next.start_all(&restored);
    let mut world = Mock::default();
    next.run(&mut world, &chat("c"), 0);
    assert_eq!(said(&world), vec!["3"], "the new bank's plugin went on from the old one's state (2), and was given its version (3)");

    // A restart: the file beside the plugin, written at a world save (checkpoint, then write).
    next.checkpoint(&mut Saved::default(), None);
    next.write_states();
    assert!(state::path(&dir.join("counter"), "world").is_file());
    let mut after_restart = boot(&dir);
    let mut world = Mock::default();
    after_restart.run(&mut world, &chat("d"), 0);
    assert_eq!(said(&world), vec!["4"]);
}

#[test]
fn a_crashed_plugin_restarts_from_its_last_checkpoint() {
    let dir = fresh_dir("restart");
    // Counts, and traps when the count reaches 3 (the digit is then 3).
    let wat = module(
        "(global.set $count (i32.add (global.get $count) (i32.const 1))) (if (i32.eq (global.get $count) (i32.const 3)) (then unreachable)) (i32.store8 (i32.const 0) (i32.add (i32.const 48) (global.get $count))) (drop (call $say (i32.const 0) (i32.const 1))) (i32.const 0)",
        r#"(global $count (mut i32) (i32.const 0))
           (func (export "pcds_save") (result i64) (i32.store (i32.const 2048) (global.get $count)) (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const 4)))
           (func (export "pcds_load") (param i32 i32 i32) (result i32) (if (i32.ne (local.get 1) (i32.const 0)) (then (global.set $count (i32.load (local.get 0))))) (i32.const 0))"#,
    );
    install(&dir, "fragile", &wat, &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    let mut world = Mock::default();
    host.run(&mut world, &chat("1"), 0);
    host.run(&mut world, &chat("2"), 0);
    host.checkpoint(&mut Saved::default(), None);
    host.run(&mut world, &chat("3"), 0);
    host.run(&mut world, &chat("4"), 0);
    // 1, 2 said; the third trapped and the plugin restarted at the checkpoint (2); the fourth is the third again.
    assert_eq!(said(&world), vec!["1", "2"]);
    assert_eq!(plugin(&host, "fragile").stats.faults, 2);
}

#[test]
fn the_flip_budget_leaves_a_slow_saver_with_its_last_state() {
    let dir = fresh_dir("flipbudget");
    install(&dir, "counter", &counter(), &CHAT_SAY, SUBSCRIBE_CHAT, None);
    let mut host = boot(&dir);
    host.run(&mut Mock::default(), &chat("a"), 0);
    let mut saved = Saved::default();
    host.checkpoint(&mut saved, None);
    let first = saved.states["counter"].clone();
    host.run(&mut Mock::default(), &chat("b"), 0);
    // No time left: the plugin is not asked, and the snapshot carries the state of the save before.
    host.checkpoint(&mut saved, Some(Duration::ZERO));
    assert_eq!(saved.states["counter"], first);
    assert_eq!(plugin(&host, "counter").stats.shed, 1);
    host.checkpoint(&mut saved, Some(limits::FLIP_BUDGET));
    assert_ne!(saved.states["counter"], first);
}

#[test]
fn zero_plugins_is_an_empty_host_and_a_missing_folder_is_no_error() {
    let host = prepare(Path::new("/nonexistent/pcds/plugins"), "world");
    assert!(host.is_empty() && host.interest == 0 && host.approvals_error.is_none());
}

#[test]
fn the_admin_json_shows_what_was_asked_what_was_granted_and_what_went_wrong() {
    let dir = fresh_dir("json");
    install(&dir, "trapper", &module("unreachable", ""), &["event.chat", "chat.say", "player.kick"], SUBSCRIBE_CHAT, Some(&["event.chat", "chat.say"]));
    let mut host = boot(&dir);
    host.run(&mut Mock::default(), &chat("x"), 0);
    let j = super::admin::plugin_json(plugin(&host, "trapper"));
    assert_eq!(j["status"], "running");
    assert_eq!(j["notGranted"], serde_json::json!(["player.kick"]));
    assert_eq!(j["granted"], serde_json::json!(["event.chat", "chat.say"]));
    assert_eq!(j["stats"]["faults"], 1);
    assert!(j["stats"]["lastError"].as_str().unwrap().starts_with("chat: trap"), "{}", j["stats"]["lastError"]);
    assert_eq!(j["limits"]["fuelPerEvent"], limits::FUEL_PER_EVENT);
}

/// docs/plugins-api.md says what the code does: the kinds, the capabilities, the imports and
/// exports, the limits, the endpoints and the version. A change on one side fails here until the
/// other follows.
#[test]
fn the_api_document_matches_the_code() {
    let doc = include_str!("../../../docs/plugins-api.md");
    let has_row = |row: &str| doc.lines().any(|l| l.starts_with(row));
    for k in Kind::ALL {
        assert!(has_row(&format!("| {} | `{}` |", k as u32, k.name())), "the event table lacks kind {} `{}`", k as u32, k.name());
    }
    for (name, allows) in pcds_plugin_api::caps::ALL {
        assert!(has_row(&format!("| `{name}` | {allows}")), "the capability table lacks `{name}` | {allows}");
    }
    for import in ["log", "query", "say", "tell", "kick", "teleport", "rules_patch"] {
        assert!(has_row(&format!("| `pcds.{import}` |")), "pcds.{import}");
        assert!(super::wasm::IMPORTS.contains(&import));
    }
    assert_eq!(super::wasm::IMPORTS.len(), 7);
    for export in ["memory", "pcds_abi", "pcds_alloc", "pcds_free", "pcds_event", "pcds_load", "pcds_save", "pcds_state_version"] {
        assert!(has_row(&format!("| `{export}` |")), "export {export}");
    }
    assert!(doc.contains(&format!("`ABI {}.{}`", pcds_plugin_api::ABI_MAJOR, pcds_plugin_api::ABI_MINOR)), "the ABI version");
    assert!(doc.contains(&format!("`handoff::FORMAT` {}", crate::handoff::FORMAT)), "the snapshot format the plugin part is added to");
    let limit_rows = [
        ("FUEL_PER_EVENT", limits::FUEL_PER_EVENT),
        ("MAX_FUEL_PER_EVENT", limits::MAX_FUEL_PER_EVENT),
        ("FUEL_PER_SAVE", limits::FUEL_PER_SAVE),
        ("MAX_FUEL_PER_SAVE", limits::MAX_FUEL_PER_SAVE),
        ("FUEL_LOAD", limits::FUEL_LOAD),
        ("MEMORY_PAGES", limits::MEMORY_PAGES as u64),
        ("MAX_MEMORY_PAGES", limits::MAX_MEMORY_PAGES as u64),
        ("MAX_MODULE_BYTES", limits::MAX_MODULE_BYTES as u64),
        ("TICK_EVERY_MS", limits::TICK_EVERY_MS as u64),
        ("MIN_TICK_MS", limits::MIN_TICK_MS as u64),
        ("ACTIONS_PER_CALL", limits::ACTIONS_PER_CALL as u64),
        ("ACTIONS_PER_SECOND", limits::ACTIONS_PER_SECOND as u64),
        ("RULE_CHANGES_PER_SECOND", limits::RULE_CHANGES_PER_SECOND as u64),
        ("LOG_LINES_PER_SECOND", limits::LOG_LINES_PER_SECOND as u64),
        ("LOG_LINE_CHARS", limits::LOG_LINE_CHARS as u64),
        ("SAY_CHARS", limits::SAY_CHARS as u64),
        ("MAX_STATE_BYTES", limits::MAX_STATE_BYTES as u64),
        ("MAX_PAYLOAD", limits::MAX_PAYLOAD as u64),
        ("MAX_FAULTS", limits::MAX_FAULTS as u64),
        ("MAX_PLUGINS", limits::MAX_PLUGINS as u64),
    ];
    for (name, value) in limit_rows {
        assert!(has_row(&format!("| `{name}` | {value} |")), "the limits table: `{name}` should be {value}");
    }
    // And the other way: every limit the document lists is one the code has.
    let section = doc.split("## Limits").nth(1).unwrap().split("\n## ").next().unwrap();
    for row in section.lines().filter(|l| l.starts_with("| `")) {
        let name = row.trim_start_matches("| `").split('`').next().unwrap();
        assert!(limit_rows.iter().any(|(n, _)| *n == name), "the document lists a limit the code does not have: {name}");
    }
    assert!(doc.contains("`TICK_BUDGET` (2 ms)") && limits::TICK_BUDGET.as_millis() == 2);
    assert!(doc.contains("`FLIP_BUDGET` (25 ms)") && limits::FLIP_BUDGET.as_millis() == 25);
    assert!(doc.contains("`SLOW_CALL` (5 ms)") && limits::SLOW_CALL.as_millis() == 5);
    assert!(doc.contains("`FAULT_WINDOW` is 60 seconds") && limits::FAULT_WINDOW.as_secs() == 60);
    let admin = include_str!("admin.rs");
    for route in ["GET /api/plugins", "POST /api/plugins/reload", "POST /api/plugins/enable", "POST /api/plugins/disable", "POST /api/plugins/events"] {
        assert!(doc.contains(&format!("`{route}`")), "{route} in the document");
        let (method, path) = route.split_once(' ').unwrap();
        assert!(admin.contains(&format!("(\"{method}\", \"{path}\")")), "{route} in admin.rs");
    }
    for status in ["running", "pending", "disabled", "changed", "invalid", "incompatible", "failed"] {
        assert!(doc.contains(&format!("`{status}`")), "status {status}");
    }
    for key in ["id", "name", "version", "description", "authors", "license", "kind", "entry", "abi", "min_server", "capabilities", "fuel_per_event", "fuel_per_save", "memory_pages", "tick_every_ms", "subscribe", "rpc"] {
        assert!(doc.contains(&format!("\n{key} = ")) || doc.contains(&format!("\n{key} =")), "manifest key {key}");
    }
    for var in ["name", "client", "players", "planet", "time", "args", "arg1", "arg2", "arg3"] {
        assert!(doc.contains(&format!("`{{{var}}}`")), "variable {var}");
    }
    // The guide's steps use commands that exist.
    for cmd in ["list", "check", "install", "enable", "disable", "reload", "new"] {
        assert!(include_str!("../cli/plugin.rs").contains(&format!("[\"{cmd}\"")) || include_str!("../cli/plugin.rs").contains(&format!("[\"{cmd}\",")), "pcds plugin {cmd}");
    }
}
