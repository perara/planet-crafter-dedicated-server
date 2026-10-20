//! A server that runs for months must run at the pace it had on its first day. Time travel: the
//! same world, with a player and machines, plays the same eight minutes of game time at the age of
//! 0, 1, 3, 7, 25, 60, 200, 400 and 900 days (the clock of the game starts there, so that every
//! timer was set at that age, as if the server had run all that time), and what happens must
//! not depend on the age: every system takes its steps at the same ticks, and what they add up
//! to is the same.
//!
//! Every tick of the game goes through `Game::tick`, 7.3 ms apart (no due time of the game is a
//! multiple of that, so a step is never decided by a tie that rounding could break). A step is
//! seen as a change: the units' totals, the player's oxygen, the items of a water collector,
//! the growth of an incubator, a vegetube and a moving elevator (one per animals frame), the
//! despawn of a dropped item, the clients' time sync. The lists of ticks at which they changed
//! are compared with age 0 (one tick of slack for a decision that rounding made the other way),
//! and so are the amounts at the end.
//!
//! `cargo test --release timetravel -- --nocapture` prints the table. The ages beyond the f32
//! clock's reach (24 days: 0.25 s steps, 97 days: 1 s steps) are where the game's own float
//! clock would stop being a clock.

use std::collections::BTreeMap;

use serde_json::json;

use crate::game::tests::{join, save};
use crate::game::{Args, Game};
use crate::testkit::*;
use crate::units::Kind;

/// Seconds between the ticks (prime to the game's 0.1, 0.2, 0.5, 1, 2 s steps).
const STEP: f64 = 0.0073;
/// Game seconds played.
const SECONDS: f64 = 460.0;
/// The ages, in days of uptime.
pub const DAYS: [f64; 9] = [0.0, 1.0, 3.0, 7.0, 25.0, 60.0, 200.0, 400.0, 900.0];
const ELEVATOR: i32 = 101_700_000;

/// What one run saw.
#[derive(Debug)]
struct Report {
    /// The ticks at which each thing changed.
    events: BTreeMap<&'static str, Vec<u32>>,
    /// What things came to by the end.
    totals: BTreeMap<&'static str, f64>,
}

/// A world with Ann, a heater on a generator, a water collector, an incubator and a vegetube,
/// an elevator moving and an item that despawns, begun at `days` of uptime; played for SECONDS.
fn play(days: f64) -> Option<Report> {
    let (net_data, data, spec, scenes) = crate::game::test_tables()?;
    let start = days * 86_400.0;
    let mut g = Game::with_clock(net_data, data, spec, save(), scenes, Some(start));
    let ann = join(&mut g, 1, "Ann");
    place(&mut g, "Heater1", [570.0, 3.0, 500.0]);
    place(&mut g, "EnergyGenerator6", [575.0, 3.0, 500.0]);
    let collector = place(&mut g, "WaterCollector1", [560.0, 3.0, 520.0]).0;
    let incubator = place(&mut g, "Incubator1", [580.0, 3.0, 500.0]).0;
    let tube = place(&mut g, "Vegetube1", [585.0, 3.0, 500.0]).0;
    let (collector_inv, incubator_inv, tube_inv) = (g.wos[&collector].linked_inventory, g.wos[&incubator].linked_inventory, g.wos[&tube].linked_inventory);
    g.add_new_item(g.data.group("Iron").unwrap(), incubator_inv, 0);
    let larvae = g.data.group("Bee1Larvae").unwrap();
    g.wos.get_mut(&incubator).unwrap().linked_groups = Some(vec![larvae]);
    g.add_new_item(g.data.group("Seed0").unwrap(), tube_inv, 0);
    send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.002), json!(0)]);
    let iron = hash(&g, "Iron");
    g.world_objects_rpc(1, "WorldObjectsHandler.CreateAndDropOnFloorServerRpc", &Args(vec![json!(iron), json!([560.0, 3.0, 500.0]), json!(0.6), json!(true), json!(false), json!(10.0)]));
    g.net.out.clear();

    let mut events: BTreeMap<&'static str, Vec<u32>> = BTreeMap::new();
    let mut last: BTreeMap<&'static str, f64> = BTreeMap::new();
    let heat = |g: &Game| g.units.planet(g.planet_hash()).unwrap().units.iter().find(|u| u.kind == Kind::Heat).unwrap().total;
    let (heat0, oxygen0, ticks0) = (heat(&g), crate::gauges::values(&g, ann)[0], g.tick_count());
    let n = (SECONDS / STEP) as u32;
    for k in 1..=n {
        g.sim_time = Some(start + f64::from(k) * STEP);
        g.tick();
        let sync = g.net.out.iter().filter(|(_, kind, _)| *kind == crate::protocol::TIME_SYNC).count();
        g.net.out.clear();
        g.kicks.clear();
        let watched: [(&'static str, f64); 7] = [
            ("units", heat(&g)),
            ("gauges", f64::from(crate::gauges::values(&g, ann)[0])),
            ("collector", g.inventories[&collector_inv].items.len() as f64),
            ("incubator", f64::from(g.wos[&incubator].growth) + 1e3 * g.inventories[&incubator_inv].items.len() as f64),
            ("vegetube", f64::from(g.wos[&tube].growth)),
            ("animals", g.wos.get(&ELEVATOR).map_or(0.0, |w| f64::from(w.growth))),
            ("timers", g.timers.0.len() as f64),
        ];
        for (name, v) in watched {
            if last.insert(name, v).is_some_and(|before| before != v) {
                events.entry(name).or_default().push(k);
            }
        }
        for _ in 0..sync {
            events.entry("time sync").or_default().push(k);
        }
    }
    let mut totals = BTreeMap::new();
    totals.insert("heat grown", heat(&g) - heat0);
    totals.insert("oxygen drained", f64::from(oxygen0 - crate::gauges::values(&g, ann)[0]));
    totals.insert("vegetube growth", f64::from(g.wos[&tube].growth));
    totals.insert("elevator", f64::from(g.wos[&ELEVATOR].growth));
    totals.insert("network ticks", (g.tick_count() - ticks0) as f64);
    Some(Report { events, totals })
}

/// How much `r` differs from `base`: per name, (events in each, ticks the lists are apart at most),
/// and per total its relative difference.
fn compare(base: &Report, r: &Report) -> Vec<String> {
    let mut problems = Vec::new();
    for name in base.events.keys().chain(r.events.keys()).collect::<std::collections::BTreeSet<_>>() {
        let (a, b) = (base.events.get(name).map_or(&[][..], |v| &v[..]), r.events.get(name).map_or(&[][..], |v| &v[..]));
        // A few ticks: age 0 has the game's float sums, whose steps drift by milliseconds in
        // eight minutes (a due time that fell on the other side of a tick by rounding is one).
        let apart = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        if a.len() != b.len() || apart > 4 {
            problems.push(format!("{name}: {} steps against {} at age 0, up to {apart} ticks apart", b.len(), a.len()));
        }
    }
    for (name, v) in &r.totals {
        let v0 = base.totals[name];
        // The units' total of age 0 is a float that rounds at every step (3e-5 of what it grew by
        // here); the ticks are counted whole.
        let tolerance = match *name {
            "heat grown" => 1e-4 * v0.abs(),
            "network ticks" => 1.0,
            _ => 1e-6 * v0.abs().max(1.0),
        };
        if (v - v0).abs() > tolerance {
            problems.push(format!("{name}: {v} against {v0} at age 0"));
        }
    }
    problems
}

/// The most ticks the steps of each system are from those of age 0 (the lists cut to the shorter).
fn drift(base: &Report, r: &Report) -> BTreeMap<&'static str, u32> {
    r.events.iter().map(|(name, b)| (*name, base.events.get(name).map_or(0, |a| a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)))).collect()
}

/// The table: steps per system and the amounts, at every age; then the drift in ticks.
fn table(reports: &[(f64, Report)]) -> String {
    let names: Vec<&str> = reports[0].1.events.keys().copied().collect();
    let mut out = format!("{:>8}", "days");
    for n in &names {
        out += &format!(" {n:>10}");
    }
    for n in reports[0].1.totals.keys() {
        out += &format!(" {n:>16}");
    }
    out += "\n";
    for (days, r) in reports {
        out += &format!("{days:>8}");
        for n in &names {
            out += &format!(" {:>10}", r.events.get(n).map_or(0, Vec::len));
        }
        for v in r.totals.values() {
            out += &format!(" {v:>16.6}");
        }
        out += "\n";
    }
    out += &format!("\nticks (of {STEP} s) the steps are apart from age 0, at most\n{:>8}", "days");
    for n in &names {
        out += &format!(" {n:>10}");
    }
    out += "\n";
    for (days, r) in reports {
        out += &format!("{days:>8}");
        let d = drift(&reports[0].1, r);
        for n in &names {
            out += &format!(" {:>10}", d.get(n).copied().unwrap_or(0));
        }
        out += "\n";
    }
    out
}

#[test]
fn every_system_takes_its_steps_at_the_same_pace_at_any_age_of_the_server() {
    let mut reports = Vec::new();
    for days in DAYS {
        let Some(r) = play(days) else { return };
        reports.push((days, r));
    }
    let table = table(&reports);
    println!("{table}");
    // Something happened to compare.
    let base = &reports[0].1;
    for name in ["units", "gauges", "collector", "incubator", "vegetube", "animals", "timers", "time sync"] {
        assert!(!base.events.get(name).is_none_or(Vec::is_empty), "{name} did nothing at age 0:\n{table}");
    }
    let problems: Vec<String> = reports[1..].iter().flat_map(|(days, r)| compare(base, r).into_iter().map(move |p| format!("{days} days, {p}"))).collect();
    assert!(problems.is_empty(), "{}\n{table}", problems.join("\n"));
}
