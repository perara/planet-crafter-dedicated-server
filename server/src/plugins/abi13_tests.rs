//! The host against what ABI 1.3 added, with hostile modules written as WebAssembly text and a
//! stand-in world (no game tables): the queries with arguments, `give`, `tokens`, timers, settings, the
//! world in `load`, commands with a permission and a shape, the messages of faults, and what the
//! listing says. A module is asked one thing and answers with the verdict: 1 (a veto) means "the call
//! returned the code I expected", so the test reads the host's own answer to the module.

use pcds_plugin_api::query::{InventoryInfo, ItemCount, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};
use pcds_plugin_api::{Event, Kind, Verdict};

use super::ctx::{Action, World};
use super::tests::{fresh_dir, install};
use super::*;

/// A world with some players, a backpack, some objects, and a clock, that records what was applied.
#[derive(Default)]
pub(super) struct Stage {
    pub(super) applied: Vec<(String, Action)>,
    pub(super) objects: u32,
    pub(super) operators: Vec<u64>,
}

impl World for Stage {
    fn info(&self) -> WorldInfo {
        WorldInfo { planet: "Prime".into(), players: 2, game_ms: 61_000, unix_ms: 1_760_000_000_000, command_prefix: "!".into(), world_objects: self.objects, ..Default::default() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        vec![PlayerInfo { client: 1, name: "Ann".into(), player_id: 1001, operator: self.operators.contains(&1), ..Default::default() }, PlayerInfo { client: 2, name: "Bo".into(), player_id: 1002, operator: self.operators.contains(&2), ..Default::default() }]
    }
    fn rules(&self) -> String {
        "{}".into()
    }
    fn inventory(&self, client: u64) -> Option<InventoryInfo> {
        (client == 1).then(|| InventoryInfo { slots_used: 2, slots_total: 30, items: vec![ItemCount { group: "Iron".into(), count: 2 }] })
    }
    fn nearby(&self, center: [f32; 3], radius: f32, max: usize) -> (Vec<Nearby>, usize) {
        let all = [ObjectInfo { id: 7, net: 70, group: "Drill1".into(), position: [1.0, 0.0, 0.0] }, ObjectInfo { id: 8, net: 80, group: "Container1".into(), position: [50.0, 0.0, 0.0] }];
        let mut found: Vec<Nearby> = all.iter().map(|o| Nearby { object: o.clone(), distance: (0..3).map(|i| (o.position[i] - center[i]).powi(2)).sum::<f32>().sqrt() }).filter(|n| n.distance <= radius).collect();
        found.truncate(max);
        (found, self.objects as usize)
    }
    fn object(&self, which: ObjectRef) -> Option<ObjectInfo> {
        match which {
            ObjectRef::World(7) | ObjectRef::Net(70) => Some(ObjectInfo { id: 7, net: 70, group: "Drill1".into(), position: [1.0, 0.0, 0.0] }),
            _ => None,
        }
    }
}

impl Sink for Stage {
    fn apply(&mut self, plugin: &str, queued: Vec<Action>) -> Applied {
        let n = queued.len() as u32;
        self.applied.extend(queued.into_iter().map(|a| (plugin.to_string(), a)));
        Applied { done: n, refused: 0 }
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
    fn operator(&self, client: u64) -> bool {
        self.operators.contains(&client)
    }
}

/// A module with every import of ABI 1.3. `body` runs in `pcds_event` (it may use $kind $ptr $len and
/// the helpers); `load` runs in `pcds_load`; `extra` adds functions. Memory: "Iron" at 0, a client number
/// (1) as 8 bytes at 16, a nearby argument at 32 (center 0,0,0, radius 10, max 5), an object ref at 64.
fn module13(body: &str, load: &str, extra: &str) -> String {
    format!(
        r#"(module
          (import "pcds" "log" (func $log (param i32 i32 i32)))
          (import "pcds" "query" (func $query (param i32 i32 i32 i32 i32) (result i32)))
          (import "pcds" "say" (func $say (param i32 i32) (result i32)))
          (import "pcds" "tell" (func $tell (param i64 i32 i32) (result i32)))
          (import "pcds" "give" (func $give (param i64 i32 i32 i32) (result i32)))
          (import "pcds" "tokens" (func $tokens (param i32) (result i32)))
          (import "pcds" "timer_set" (func $timer_set (param i32 i32 i32) (result i32)))
          (import "pcds" "timer_cancel" (func $timer_cancel (param i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "Iron")
          (data (i32.const 16) "\01\00\00\00\00\00\00\00")
          (data (i32.const 32) "\00\00\00\00\00\00\00\00\00\00\00\00\00\00\20\41\05\00\00\00")
          (data (i32.const 64) "\00\46\00\00\00\00\00\00\00")
          (data (i32.const 128) "hello")
          (global $seen (mut i32) (i32.const 0))
          (func (export "pcds_abi") (result i32) (i32.const 65539))
          (func (export "pcds_alloc") (param i32) (result i32) (i32.const 4096))
          (func (export "pcds_free") (param i32 i32))
          (func (export "pcds_load") (param i32 i32 i32) (result i32) {load} (i32.const 0))
          (func (export "pcds_event") (param $kind i32) (param $ptr i32) (param $len i32) (result i32) {body})
          {extra})"#
    )
}

/// The body of a module that answers 1 when `call` returned `code`, else 0.
fn expects(call: &str, code: i32) -> String {
    format!("(if (i32.eq {call} (i32.const {code})) (then (return (i32.const 1)))) (i32.const 0)")
}

const RPC: &str = "[events]\nsubscribe = [\"rpc_before\"]\nrpc = [\"X.Y\"]";
const ALL: [&str; 9] = ["event.rpc", "rpc.veto", "world.read", "chat.say", "player.give", "world.tokens", "event.timer", "event.tick", "event.chat"];

fn probe(id: &str, grant: &[&str], body: &str) -> (Host, Stage, PathBuf) {
    let dir = fresh_dir(id);
    install(&dir, id, &module13(body, "", ""), &ALL, RPC, Some(grant));
    (super::tests::boot(&dir), Stage::default(), dir)
}

fn rpc() -> Event<'static> {
    Event::RpcBefore { client: 1, rpc: "X.Y", object: 5, args_json: "[]" }
}

fn answers(host: &mut Host, stage: &mut Stage) -> bool {
    host.run(stage, &rpc(), 61_000) == Verdict::Veto
}

use std::path::PathBuf;

const EVERYTHING: [&str; 7] = ["event.rpc", "rpc.veto", "world.read", "player.give", "world.tokens", "event.timer", "chat.say"];

#[test]
fn give_and_tokens_check_their_arguments_their_capability_and_their_rate() {
    // A call: (give client group_ptr group_len count)
    let give = |ptr: i32, len: i32, count: i32| format!("(call $give (i64.const 1) (i32.const {ptr}) (i32.const {len}) (i32.const {count}))");
    for (what, call, code) in [
        ("a good give", give(0, 4, 10), 0),
        ("a count of 0", give(0, 4, 0), -2),
        ("a count over 50", give(0, 4, 51), -2),
        ("a name of 0 bytes", give(0, 0, 1), -2),
        ("a name that is not an item id", give(128, 5, 1), 0), // "hello" is a good id shape; the game decides at apply time
        ("a name over 64 bytes", give(0, 65, 1), -2),
    ] {
        let (mut host, mut stage, _) = probe("g", &EVERYTHING, &expects(&call, code));
        assert!(answers(&mut host, &mut stage), "{what}");
    }
    // Not granted: -1, and the host says which capability, once.
    let (mut host, mut stage, _) = probe("ng", &["event.rpc", "rpc.veto"], &expects(&give(0, 4, 1), -1));
    assert!(answers(&mut host, &mut stage));
    assert!(answers(&mut host, &mut stage));
    let p = &host.plugins[0];
    assert_eq!(p.denied_caps, vec!["player.give"]);
    assert_eq!(p.logs.last(10).iter().filter(|l| l.text.contains("player.give")).count(), 1, "said once: {:?}", p.logs.last(10));
    assert!(stage.applied.is_empty());
    // The rate: ten a second, then -3; and what was queued is applied after the call.
    let burst: String = (0..11).map(|_| format!("(drop {})", give(0, 4, 1))).collect::<Vec<_>>().join(" ");
    let (mut host, mut stage, _) = probe("rate", &EVERYTHING, &format!("{burst} {}", expects(&give(0, 4, 1), -3)));
    assert!(answers(&mut host, &mut stage), "the 12th give in a second is refused");
    assert_eq!(stage.applied.len(), limits::ACTIONS_PER_CALL.min(10), "ten gives were queued; the rest not");
    assert!(matches!(&stage.applied[0].1, Action::Give(1, g, 1) if g == "Iron"));
    // Tokens.
    for (what, call, code) in [("a good amount", "(call $tokens (i32.const 500))", 0), ("a negative amount", "(call $tokens (i32.const -500))", 0), ("zero", "(call $tokens (i32.const 0))", -2), ("over the most", "(call $tokens (i32.const 10001))", -2), ("below the least", "(call $tokens (i32.const -10001))", -2), ("i32 min", "(call $tokens (i32.const -2147483648))", -2)] {
        let (mut host, mut stage, _) = probe("t", &EVERYTHING, &expects(call, code));
        assert!(answers(&mut host, &mut stage), "{what}");
    }
    let (mut host, mut stage, _) = probe("trate", &EVERYTHING, &format!("(drop (call $tokens (i32.const 1))) (drop (call $tokens (i32.const 1))) {}", expects("(call $tokens (i32.const 1))", -3)));
    assert!(answers(&mut host, &mut stage), "two token changes a second");
    let (mut host, mut stage, _) = probe("tn", &["event.rpc", "rpc.veto"], &expects("(call $tokens (i32.const 1))", -1));
    assert!(answers(&mut host, &mut stage));
    assert_eq!(host.plugins[0].denied_caps, vec!["world.tokens"]);
}

#[test]
fn queries_with_arguments_are_checked_and_paid_for() {
    let q = |kind: i32, arg: i32, len: i32| format!("(call $query (i32.const {kind}) (i32.const {arg}) (i32.const {len}) (i32.const 1024) (i32.const 512))");
    let answered = |call: String| format!("(if (i32.gt_s {call} (i32.const 0)) (then (return (i32.const 1)))) (i32.const 0)");
    // Q_INVENTORY (4) of client 1: found; of nobody: -6; with no argument: -2; with an argument that is too long: -2.
    for (what, body) in [
        ("the backpack of a player", answered(q(4, 16, 8))),
        ("an unknown player", expects(&q(4, 128, 8), -6)),
        ("no argument", expects(&q(4, 0, 0), -2)),
        ("an argument of the wrong size", expects(&q(4, 16, 3), -2)),
        ("an argument over the limit", expects(&q(4, 16, 65), -2)),
        ("nearby", answered(q(5, 32, 20))),
        ("nearby with a short argument", expects(&q(5, 32, 4), -2)),
        ("an object", answered(q(6, 64, 9))),
        ("an unknown query", expects(&q(99, 0, 0), -4)),
        ("one player", answered(q(8, 16, 8))),
        ("an offline player", expects(&q(8, 128, 8), -6)),
        ("the settings, with no capability needed", answered(q(7, 0, 0))),
    ] {
        let grant: &[&str] = if what.starts_with("the settings") { &["event.rpc", "rpc.veto"] } else { &EVERYTHING };
        let (mut host, mut stage, _) = probe("q", grant, &body);
        assert!(answers(&mut host, &mut stage), "{what}");
    }
    // Without world.read the world's queries are -1 (and the settings still answer).
    let (mut host, mut stage, _) = probe("noread", &["event.rpc", "rpc.veto"], &expects(&q(4, 16, 8), -1));
    assert!(answers(&mut host, &mut stage));
    // A pointer outside the memory is a trap, as everywhere.
    let (mut host, mut stage, _) = probe("trap", &EVERYTHING, &format!("(drop {}) (i32.const 0)", q(4, 70000, 8)));
    assert!(!answers(&mut host, &mut stage));
    assert_eq!(host.plugins[0].stats.faults, 1, "{:?}", host.plugins[0].stats.last_error);
}

#[test]
fn a_scan_of_the_world_costs_fuel_in_proportion_to_the_world() {
    // The same query in a small world and in one of 100,000 objects: the host's scan is the plugin's to pay for.
    let call = "(drop (call $query (i32.const 5) (i32.const 32) (i32.const 20) (i32.const 1024) (i32.const 512)))";
    let cost = |objects: u32| {
        let (mut host, mut stage, _) = probe("scan", &EVERYTHING, &format!("{call} (i32.const 0)"));
        stage.objects = objects;
        host.run(&mut stage, &rpc(), 61_000);
        host.plugins[0].stats.fuel
    };
    let (small, big) = (cost(10), cost(100_000));
    assert!(big > small + 100_000 * limits::FUEL_PER_SCANNED_OBJECT / 2, "{small} vs {big}");
    // And a world too big to scan inside the budget faults the plugin instead of stalling the tick.
    let (mut host, mut stage, _) = probe("scanfault", &EVERYTHING, &format!("{call} (i32.const 0)"));
    stage.objects = 5_000_000;
    host.run(&mut stage, &rpc(), 61_000);
    assert_eq!(host.plugins[0].stats.faults, 1);
    assert!(host.plugins[0].stats.last_error.as_deref().unwrap().contains("out of fuel"), "{:?}", host.plugins[0].stats.last_error);
}

#[test]
fn timers_fire_in_game_time_are_bounded_and_die_with_the_instance() {
    let dir = fresh_dir("timers");
    // Event timer: counts itself in a global and says so; load sets timers 1 (once, 1 s) and 2 (every 500 ms).
    let load = "(drop (call $timer_set (i32.const 1) (i32.const 1000) (i32.const 0))) (drop (call $timer_set (i32.const 2) (i32.const 500) (i32.const 500)))";
    let body = "(if (i32.eq (local.get $kind) (i32.const 13)) (then (drop (call $tell (i64.const 1) (i32.const 128) (i32.const 5))))) (i32.const 0)";
    install(&dir, "timed", &module13(body, load, ""), &["event.timer", "chat.say"], "", None);
    let mut host = super::tests::boot(&dir);
    let mut stage = Stage::default();
    let tick = |ms: i64| Event::Tick { game_ms: ms, tick: 0, players: 0, since_ms: 5, unix_ms: 0 };
    let told = |s: &Stage| s.applied.iter().filter(|(_, a)| matches!(a, Action::Tell(..))).count();
    assert!(host.interest & Kind::Tick.bit() != 0, "a plugin that may set timers wants the tick");
    // The timers began at the first tick; before they are due, nothing.
    host.run(&mut stage, &tick(10_000), 10_000);
    assert_eq!(told(&stage), 0);
    host.run(&mut stage, &tick(10_499), 10_499);
    assert_eq!(told(&stage), 0);
    host.run(&mut stage, &tick(10_500), 10_500);
    assert_eq!(told(&stage), 1, "timer 2 at 500 ms");
    host.run(&mut stage, &tick(11_000), 11_000);
    assert_eq!(told(&stage), 3, "timer 1 once and timer 2 again");
    host.run(&mut stage, &tick(11_500), 11_500);
    assert_eq!(told(&stage), 4, "the one-shot is gone, the repeating one goes on");
    // A long gap fires a repeating timer once, not once for every repeat it missed.
    host.run(&mut stage, &tick(900_000), 900_000);
    assert_eq!(told(&stage), 5);
    assert_eq!(host.plugins[0].stats.by_event[Kind::Timer as usize].calls, 5);
    // A new instance has none until its load sets them: after a fault the instance is made again, and its load sets them.
    assert_eq!(host.plugins[0].rt.as_mut().unwrap().ctx().timers.len(), 1, "the one-shot is gone");
    host.plugins[0].faulted(&runtime::Fault::Trap("test".into()), "test");
    assert_eq!(host.plugins[0].rt.as_mut().unwrap().ctx().timers.len(), 2, "the load set them again");
    // Not granted: the module's timer calls are refused and nothing is ever delivered.
    let dir = fresh_dir("timers-denied");
    install(&dir, "timed", &module13(body, load, ""), &["event.timer", "chat.say"], "", Some(&["chat.say"]));
    let mut host = super::tests::boot(&dir);
    assert_eq!(host.plugins[0].rt.as_mut().unwrap().ctx().timers.len(), 0);
    assert_eq!(host.interest & Kind::Tick.bit(), 0);
}

#[test]
fn the_timer_calls_check_their_arguments_and_their_number() {
    for (what, call, code) in [
        ("a good timer", "(call $timer_set (i32.const 1) (i32.const 1000) (i32.const 0))", 0),
        ("a repeat under 100 ms", "(call $timer_set (i32.const 1) (i32.const 1000) (i32.const 99))", -2),
        ("a delay over seven days", "(call $timer_set (i32.const 1) (i32.const 604800001) (i32.const 0))", -2),
        ("a repeat over seven days", "(call $timer_set (i32.const 1) (i32.const 1) (i32.const 604800001))", -2),
        ("cancelling one that is not there", "(call $timer_cancel (i32.const 77))", 0),
    ] {
        let (mut host, mut stage, _) = probe("tm", &EVERYTHING, &expects(call, code));
        assert!(answers(&mut host, &mut stage), "{what}");
    }
    // The 17th is refused; replacing one of the 16 is not.
    let sixteen: String = (0..16).map(|i| format!("(drop (call $timer_set (i32.const {i}) (i32.const 1000) (i32.const 0)))")).collect();
    let (mut host, mut stage, _) = probe("tm17", &EVERYTHING, &format!("{sixteen} {}", expects("(call $timer_set (i32.const 99) (i32.const 1000) (i32.const 0))", -3)));
    assert!(answers(&mut host, &mut stage), "the 17th is refused");
    assert_eq!(host.plugins[0].rt.as_mut().unwrap().ctx().timers.len(), 16);
    let (mut host, mut stage, _) = probe("tm16", &EVERYTHING, &format!("{sixteen} {}", expects("(call $timer_set (i32.const 3) (i32.const 5) (i32.const 0))", 0)));
    assert!(answers(&mut host, &mut stage), "replacing one of the 16 is not");
}

#[test]
fn a_plugin_can_read_the_world_while_it_loads_and_what_it_asks_for_is_applied() {
    let dir = fresh_dir("load-world");
    // load: the game time from the world query into memory, then say it (a say in load is applied once the game is free).
    let load = "(drop (call $query (i32.const 1) (i32.const 0) (i32.const 0) (i32.const 1024) (i32.const 512))) (if (i32.eq (i32.load8_u (i32.const 1024)) (i32.const 0x48)) (then (drop (call $say (i32.const 128) (i32.const 5)))))";
    install(&dir, "boot", &module13("(i32.const 0)", load, ""), &["world.read", "chat.say"], "", None);
    let mut host = prepare(&dir, "world");
    let mut stage = Stage::default();
    host.start_all_in(&Default::default(), &stage, 61_000);
    assert!(host.plugins[0].running(), "{:?}", host.plugins[0].status);
    host.flush_boot(&mut stage);
    // game_ms 61000 is 0xEE48: its first byte, 0x48, is what the module saw; it said so, and the host applied it after load.
    assert_eq!(stage.applied.len(), 1);
    assert!(matches!(&stage.applied[0].1, Action::Say(t) if t == "hello"));
    // Without the world (the plain start) the same module's query answers -4 (unsupported) and it still starts.
    let mut plain = prepare(&dir, "world");
    plain.start_all(&Default::default());
    assert!(plain.plugins[0].running());
}

#[test]
fn a_command_is_checked_for_its_permission_and_its_shape_before_the_plugin_hears_of_it() {
    let dir = fresh_dir("commands");
    let extra = "[events]\nsubscribe = []\n\n[[command]]\nname = \"kickit\"\nusage = \"<player> [reason]\"\nmin_args = 1\nmax_args = 2\npermission = \"operator\"\n\n[[command]]\nname = \"plain\"\n";
    // The plugin says "hello" to client 1 for every command it is given.
    let body = "(drop (call $tell (i64.const 1) (i32.const 128) (i32.const 5))) (i32.const 0)";
    install(&dir, "cmds", &module13(body, "", ""), &["event.chat", "chat.say"], extra, None);
    let mut host = super::tests::boot(&dir);
    let mut stage = Stage::default();
    let chat = |text: &'static str| Event::Chat { client: 1, name: "Ann", text, player_id: 1001 };
    let told = |s: &Stage| s.applied.iter().filter_map(|(_, a)| if let Action::Tell(_, t) = a { Some(t.clone()) } else { None }).collect::<Vec<_>>();
    // Not an operator: told so by the host; the plugin is not called.
    host.deliver(&mut stage, &chat("!kickit Bo"), 61_000);
    assert_eq!(told(&stage), vec!["!kickit is for the server's operators."]);
    assert_eq!(host.plugins[0].stats.calls, 0);
    // An operator, with the wrong number of arguments: the usage line.
    stage.operators.push(1);
    stage.applied.clear();
    host.deliver(&mut stage, &chat("!kickit"), 61_000);
    host.deliver(&mut stage, &chat("!kickit a b c"), 61_000);
    assert_eq!(told(&stage), vec!["Use: !kickit <player> [reason]", "Use: !kickit <player> [reason]"]);
    assert_eq!(host.plugins[0].stats.calls, 0);
    // Right: the plugin is called (and says hello); case does not matter to the lookup.
    stage.applied.clear();
    host.deliver(&mut stage, &chat("!KICKIT Bo why"), 61_000);
    assert_eq!(host.plugins[0].stats.calls, 1);
    assert_eq!(told(&stage), vec!["hello"]);
    // A command with no rules is delivered as it is.
    host.deliver(&mut stage, &chat("!plain a b c d"), 61_000);
    assert_eq!(host.plugins[0].stats.calls, 2);
}

#[test]
fn a_fault_says_what_ran_out_and_what_to_do_and_is_kept_in_the_history() {
    let dir = fresh_dir("fuel-message");
    install(&dir, "greedy", &module13("(loop $l (br $l)) (i32.const 0)", "", ""), &ALL, RPC, None);
    let mut host = super::tests::boot(&dir);
    let mut stage = Stage::default();
    host.run(&mut stage, &rpc(), 61_000);
    let p = &host.plugins[0];
    let said = p.stats.last_error.clone().unwrap();
    assert!(said.contains("rpc_before") && said.contains("1000000 fuel") && said.contains("budget.fuel_per_event") && said.contains("20000000"), "{said}");
    assert_eq!(p.stats.fault_log.len(), 1);
    assert!(p.logs.last(5).iter().any(|l| l.text.contains("out of fuel")), "the plugin's own history has it");
    // A panic compiled to `unreachable` is called one.
    let dir = fresh_dir("unreachable-message");
    install(&dir, "panicky", &module13("unreachable", "", ""), &ALL, RPC, None);
    let mut host = super::tests::boot(&dir);
    host.run(&mut stage, &rpc(), 61_000);
    assert!(host.plugins[0].stats.last_error.as_deref().unwrap().contains("a panic in Rust compiles to this"), "{:?}", host.plugins[0].stats.last_error);
}

#[test]
fn a_module_is_translated_when_read_so_its_first_call_costs_what_the_next_does() {
    let dir = fresh_dir("eager");
    // A function with a body of some size, called by the event: lazily translated, its first call would cost more.
    let work: String = "(local.set $i (i32.add (local.get $i) (i32.const 1)))".repeat(2000);
    let extra = format!("(func $big (result i32) (local $i i32) {work} (local.get $i))");
    install(&dir, "big", &module13("(drop (call $big)) (i32.const 0)", "", &extra), &ALL, RPC, None);
    let mut host = super::tests::boot(&dir);
    let mut stage = Stage::default();
    host.run(&mut stage, &rpc(), 61_000);
    let first = host.plugins[0].stats.fuel;
    host.run(&mut stage, &rpc(), 61_000);
    let second = host.plugins[0].stats.fuel - first;
    assert_eq!(first, second, "the same fuel every time: translation is not charged to a call");
}

#[test]
fn settings_have_defaults_are_checked_and_a_stored_value_that_no_longer_fits_stops_the_plugin() {
    let dir = fresh_dir("settings");
    let extra = r#"
[[config]]
key = "afk_minutes"
type = "int"
default = 15
min = 1
max = 240

[[config]]
key = "mode"
type = "string"
secret = false
default = "warn"
choices = ["warn", "kick"]

[[config]]
key = "hook"
description = "a secret"
"#;
    install(&dir, "cfg", &module13("(i32.const 0)", "", ""), &["event.rpc", "rpc.veto"], &format!("{RPC}\n{extra}"), None);
    // Defaults are the settings when nothing is set; a secret is never among them.
    let host = super::tests::boot(&dir);
    assert_eq!(*host.plugins[0].settings, vec![("afk_minutes".to_string(), "15".to_string()), ("mode".to_string(), "warn".to_string())]);
    // What the admin sets is checked against the declaration, and kept in the form the plugin reads.
    let manifest = host.plugins[0].manifest.clone().unwrap();
    let key = |name: &str| manifest.config.iter().find(|k| k.key == name).unwrap().clone();
    assert_eq!(key("afk_minutes").check(" 20 ").unwrap(), "20");
    for (name, value, why) in [("afk_minutes", "0", "below the least"), ("afk_minutes", "241", "above the most"), ("afk_minutes", "ten", "not a whole number"), ("mode", "ban", "not one of: warn, kick")] {
        let e = key(name).check(value).unwrap_err();
        assert!(e.contains(why), "{name} {value}: {e}");
    }
    // A stored value that the declaration refuses (the plugin was updated to a narrower range) is not replaced by the default.
    super::config::set(&dir, "cfg", "afk_minutes", Some("500")).unwrap();
    let host = super::tests::boot(&dir);
    assert!(matches!(&host.plugins[0].status, Status::Invalid(why) if why.contains("above the most") && why.contains("pcds plugin config set cfg afk_minutes")), "{:?}", host.plugins[0].status);
    // A default that breaks its own rules, a secret that is a number or has a default, a typo in a key: refused at read, with a hint.
    for (bad, why) in [("key = \"n\"\ntype = \"int\"\ndefault = 500\nmax = 10", "that is the default"), ("key = \"n\"\ntype = \"int\"\nsecret = true", "only a string can be a secret"), ("key = \"n\"\ndefault = \"x\"", "a secret has no default"), ("key = \"n\"\ntype = \"int\"\ndefault = \"x\"", "is not a int"), ("key = \"n\"\ntyp = \"int\"", "did you mean `type`")] {
        let text = format!("[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1.0.0\"\nkind = \"wasm\"\nentry = \"p.wasm\"\nabi = 1\n[[config]]\n{bad}\n");
        let e = manifest::Manifest::parse(&text).unwrap_err();
        assert!(e.contains(why), "{bad}: {e}");
    }
}

#[test]
fn an_unknown_key_or_capability_in_a_manifest_names_the_one_that_was_meant() {
    let base = "[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1.0.0\"\nkind = \"wasm\"\nentry = \"p.wasm\"\nabi = 1\n";
    for (extra, hint) in [("capabilites = []\n", "did you mean `capabilities`"), ("descripton = \"\"\n", "did you mean `description`"), ("capabilities = [\"event.chatt\"]\n", "did you mean `event.chat`"), ("capabilities = [\"chat.sai\"]\n", "did you mean `chat.say`"), ("capabilities = [\"event.chat\"]\n[events]\nsubscribe = [\"chatt\"]\n", "did you mean `chat`")] {
        let e = manifest::Manifest::parse(&format!("{base}{extra}")).unwrap_err();
        assert!(e.contains(hint), "{extra}: {e}");
    }
    // Far from anything: no guess.
    let e = manifest::Manifest::parse(&format!("{base}zzzzzzzz = 1\n")).unwrap_err();
    assert!(!e.contains("did you mean"), "{e}");
}

#[test]
fn the_listing_shows_what_a_new_version_asks_beyond_what_was_approved() {
    let dir = fresh_dir("diff");
    install(&dir, "grow", &module13("(i32.const 0)", "", ""), &["event.rpc", "rpc.veto"], RPC, None);
    let mut host = super::tests::boot(&dir);
    // The plugin is replaced on disk by a version that asks for more, and a host it may reach.
    super::tests::write_plugin(&dir, "grow", &module13("(i32.const 0)", "", ""), &["event.rpc", "rpc.veto", "chat.say", "net.http"], &format!("{RPC}\n[net]\nallow = [\"https://example.org\"]"));
    let mut saved = Default::default();
    host.rescan(&mut saved, false);
    let p = &host.plugins[0];
    assert!(p.running() && p.notice.as_deref().unwrap().contains("wait for approval"), "{:?} {:?}", p.status, p.notice);
    let json = admin::plugin_json(p);
    assert_eq!(json["changes"]["added"], serde_json::json!(["chat.say", "net.http"]));
    assert_eq!(json["changes"]["addedHosts"], serde_json::json!(["https://example.org"]));
    assert_eq!(json["changes"]["removed"], serde_json::json!([]));
    assert_eq!(json["changes"]["requested"].as_array().map(Vec::len), Some(4));
    // Approved: the diff is gone.
    approve(&dir, "grow", None).unwrap();
    host.rescan(&mut saved, true);
    assert!(admin::plugin_json(&host.plugins[0])["changes"].is_null());
}

#[test]
fn the_ring_of_a_plugins_log_is_bounded_and_the_listing_has_the_cost_by_event() {
    let dir = fresh_dir("history");
    // Logs 3 lines every event (the log is rate limited: 20 a second), and says a thing.
    let body = "(call $log (i32.const 2) (i32.const 128) (i32.const 5)) (call $log (i32.const 1) (i32.const 128) (i32.const 5)) (i32.const 0)";
    install(&dir, "chatty", &module13(body, "", ""), &ALL, RPC, None);
    let mut host = super::tests::boot(&dir);
    let mut stage = Stage::default();
    for _ in 0..500 {
        host.run(&mut stage, &rpc(), 61_000);
    }
    let lines = host.plugins[0].logs.last(1000);
    assert!(lines.len() <= limits::LOG_KEPT, "{}", lines.len());
    let json = admin::plugin_json(&host.plugins[0]);
    let by = &json["stats"]["byEvent"][0];
    assert_eq!(by["event"], "rpc_before");
    assert_eq!(by["calls"], 500);
    assert!(by["averageFuel"].as_u64().unwrap() > 0 && by["maxFuel"].as_u64().unwrap() >= by["averageFuel"].as_u64().unwrap());
}

/// A world whose view panics when a plugin asks for the players.
struct Broken(Stage);

impl World for Broken {
    fn info(&self) -> WorldInfo {
        self.0.info()
    }
    fn players(&self) -> Vec<PlayerInfo> {
        panic!("a bug in the world's view")
    }
    fn rules(&self) -> String {
        "{}".into()
    }
}

impl Sink for Broken {
    fn apply(&mut self, plugin: &str, queued: Vec<Action>) -> Applied {
        self.0.apply(plugin, queued)
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
}

#[test]
fn a_panic_in_the_hosts_own_code_while_it_serves_a_plugin_is_that_plugins_fault_and_the_game_goes_on() {
    let dir = fresh_dir("host-panic");
    let ask_players = "(drop (call $query (i32.const 2) (i32.const 0) (i32.const 0) (i32.const 1024) (i32.const 512))) (i32.const 0)";
    install(&dir, "asker", &module13(ask_players, "", ""), &ALL, RPC, None);
    install(&dir, "zgood", &module13("(i32.const 0)", "", ""), &ALL, RPC, None);
    let mut host = super::tests::boot(&dir);
    let mut broken = Broken(Stage::default());
    let verdict = host.run(&mut broken, &rpc(), 61_000);
    assert_eq!(verdict, Verdict::Continue);
    let p = &host.plugins[0];
    assert_eq!((p.stats.faults, p.stats.restarts), (1, 1), "{:?}", p.stats.last_error);
    assert!(p.stats.last_error.as_deref().unwrap().contains("a bug in the world's view") && p.stats.last_error.as_deref().unwrap().contains("restarted"));
    assert!(p.running(), "restarted, not stopped");
    // The other plugin was called in the same round, and the host asks nothing more of a broken plugin than five faults.
    assert_eq!(host.plugins[1].stats.calls, 1);
    for _ in 0..4 {
        host.run(&mut broken, &rpc(), 61_000);
    }
    assert!(!host.plugins[0].running() && matches!(&host.plugins[0].status, Status::Failed(why) if why.contains("switched off after 5 faults")));
}
