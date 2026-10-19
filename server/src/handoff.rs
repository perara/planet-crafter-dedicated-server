//! The game's whole runtime state as one versioned JSON document (the snapshot a flip carries
//! from one bank to the next), and a game rebuilt from it that goes on exactly as the old one
//! would have (docs/hot-update.md). The static tables (data/<version>/) are not in it: the new
//! process loads its own.

/// An Instant as the seconds since it, and back as that long before now: the clock goes on.
pub mod instant {
    use std::time::{Duration, Instant};

    pub fn serialize<S: serde::Serializer>(t: &Instant, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(t.elapsed().as_secs_f64())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Instant, D::Error> {
        let since: f64 = serde::Deserialize::deserialize(d)?;
        Ok(Instant::now().checked_sub(Duration::from_secs_f64(since.max(0.0))).unwrap_or_else(Instant::now))
    }
}

/// An f32 that may be infinite (JSON has no infinity: serde_json writes null), as its bits.
pub mod f32_bits {
    pub fn serialize<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(v.to_bits())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        Ok(f32::from_bits(serde::Deserialize::deserialize(d)?))
    }
}

/// A map with non-string keys (JSON object keys are strings), as a list of pairs.
pub mod pairs {
    use std::collections::HashMap;
    use std::hash::Hash;

    pub fn serialize<K: serde::Serialize, V: serde::Serialize, S: serde::Serializer>(m: &HashMap<K, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter())
    }
    pub fn deserialize<'de, K: serde::Deserialize<'de> + Eq + Hash, V: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(d: D) -> Result<HashMap<K, V>, D::Error> {
        let v: Vec<(K, V)> = serde::Deserialize::deserialize(d)?;
        Ok(v.into_iter().collect())
    }
}

use crate::data::NetData;
use crate::game::Game;
use crate::gamedata::GameData;
use crate::rpcspec::Spec;

/// The snapshot's format. A release that changes the state's shape (a field renamed, removed
/// or meaning something else; a new field only needs `#[serde(default)]`) bumps it and adds
/// the step from the previous format to `migrate`: each build reads its own and the previous.
pub const FORMAT: u32 = 3;

/// The snapshot's first line: its format, and the tables it is for (state holds indices into
/// them, so it is only valid on the same game version).
#[derive(serde::Serialize, serde::Deserialize)]
struct Head {
    format: u32,
    game_version: String,
}

/// The game's state at a tick boundary (the bank sent everything queued: net.out is empty),
/// written to `w`. An error leaves the game as it was: it goes on, and the flip is off.
pub fn snapshot_to(g: &Game, w: &mut impl std::io::Write) -> Result<(), String> {
    // JSON has no NaN or infinity (serde_json writes null, which does not read back as a
    // float): a position or rotation that is not a number stops the snapshot here, by name.
    let finite = |v: &[f32]| v.iter().all(|x| x.is_finite());
    if let Some(o) = g.net.objects.values().find(|o| !finite(&o.position) || !finite(&o.rotation)) {
        return Err(format!("network object {} is at {:?} {:?}, not a number: no snapshot", o.id, o.position, o.rotation));
    }
    if let Some(w) = g.wos.values().find(|w| !finite(&w.position) || !finite(&w.rotation)) {
        return Err(format!("world object {} is at {:?} {:?}, not a number: no snapshot", w.id, w.position, w.rotation));
    }
    let head = Head { format: FORMAT, game_version: g.net_data.game_version.clone() };
    serde_json::to_writer(&mut *w, &head).map_err(|e| e.to_string())?;
    w.write_all(b"\n").map_err(|e| e.to_string())?;
    serde_json::to_writer(w, g).map_err(|e| e.to_string())
}

#[cfg(test)]
pub fn snapshot(g: &Game) -> Result<String, String> {
    let mut out = Vec::new();
    snapshot_to(g, &mut out)?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// The previous format's state brought to this one, as firmware migrates its data on boot.
fn migrate(from: u32, state: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut state = state;
    match from {
        // 2: the world objects (an id -> object map) and the constructed and pickable sets (id
        // lists) became Slots, in their order and with no slot free yet; the units' constructed
        // index became an integer.
        1 => {
            let wos = std::mem::take(&mut state["wos"]);
            let slots: Vec<serde_json::Value> = wos.as_object().into_iter().flatten().map(|(id, wo)| serde_json::json!([id.parse::<i32>().unwrap_or(0), wo])).collect();
            state["wos"] = serde_json::json!({"slots": slots, "free": []});
            for set in ["constructed", "pickables"] {
                let ids: Vec<serde_json::Value> = state[set].as_array().into_iter().flatten().map(|id| serde_json::json!([id, null])).collect();
                state[set] = serde_json::json!({"slots": ids, "free": []});
            }
            // The constructed-objects index, a float, became a u64.
            let int = |v: &mut serde_json::Value| *v = serde_json::json!(v.as_f64().unwrap_or(0.0) as u64);
            int(&mut state["units"]["constructed_index"]);
            for p in state["units"]["planets"].as_array_mut().into_iter().flatten() {
                p["units"].as_array_mut().into_iter().flatten().for_each(|u| int(&mut u["previous_index"]));
            }
            migrate(2, state)
        }
        // 3: the inventories (an id -> inventory map) became Slots, in their order and with no
        // slot free yet.
        2 => {
            let inventories = std::mem::take(&mut state["inventories"]);
            let slots: Vec<serde_json::Value> = inventories.as_object().into_iter().flatten().map(|(id, inv)| serde_json::json!([id.parse::<i32>().unwrap_or(0), inv])).collect();
            state["inventories"] = serde_json::json!({"slots": slots, "free": []});
            Ok(state)
        }
        _ => Err(format!("snapshot format {from}: this build reads format {FORMAT} and the one before")),
    }
}

/// A game from a snapshot and this process's own tables.
#[cfg(test)]
pub fn restore(net_data: NetData, data: GameData, spec: Spec, snapshot: &str, scenes: crate::Scenes) -> Result<Game, String> {
    restore_bytes(net_data, data, spec, snapshot.as_bytes(), scenes)
}

fn split(snapshot: &[u8]) -> Result<(Head, &[u8]), String> {
    let at = snapshot.iter().position(|b| *b == b'\n').ok_or("not a snapshot")?;
    let head: Head = serde_json::from_slice(&snapshot[..at]).map_err(|e| e.to_string())?;
    Ok((head, &snapshot[at + 1..]))
}

pub fn restore_bytes(net_data: NetData, data: GameData, spec: Spec, snapshot: &[u8], scenes: crate::Scenes) -> Result<Game, String> {
    let (head, state) = split(snapshot)?;
    if head.game_version != net_data.game_version {
        return Err(format!("a snapshot on game {}'s tables, these are {}'s", head.game_version, net_data.game_version));
    }
    let mut g: Game = match head.format {
        FORMAT => serde_json::from_slice(state).map_err(|e| e.to_string())?,
        f => serde_json::from_value(migrate(f, serde_json::from_slice(state).map_err(|e| e.to_string())?)?).map_err(|e| e.to_string())?,
    };
    g.net_data = net_data;
    g.data = data;
    g.net.spec = spec;
    g.scenes = scenes;
    g.rebuild_holders();
    crate::machines::refresh_from_tables(&mut g);
    g.trace = std::env::var_os("PCDS_TRACE").is_some();
    // The tables are not quite static: Game::new scales the groups' generation by the
    // world's settings.
    g.change_generation_for_groups();
    Ok(g)
}

/// Whether a restored game holds exactly the state of the snapshot it came from: both written
/// as text and read back, compared without the two clocks (seconds since an Instant, which go
/// on) and without the order of sets and maps (a HashSet's order differs between processes).
/// Err names the first part that differs. PCDS_FLIP_VERIFY runs it at every flip (tests).
pub fn verify(g: &Game, snapshot: &[u8]) -> Result<(), String> {
    use serde_json::Value;
    fn canon(v: &mut Value) {
        match v {
            Value::Array(a) => {
                a.iter_mut().for_each(canon);
                a.sort_by_cached_key(|x| x.to_string());
            }
            Value::Object(o) => {
                o.values_mut().for_each(canon);
                o.sort_keys();
            }
            _ => {}
        }
    }
    let state = |v: &mut Value| {
        if let Some(o) = v.as_object_mut() {
            o.remove("started");
        }
        for c in v["clients"].as_object_mut().into_iter().flat_map(|c| c.values_mut()) {
            c.as_object_mut().map(|c| c.remove("since"));
        }
        canon(v);
    };
    let mut sent: Value = serde_json::from_slice(split(snapshot)?.1).map_err(|e| e.to_string())?;
    let mut restored: Value = serde_json::from_str(&serde_json::to_string(g).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    state(&mut sent);
    state(&mut restored);
    if sent == restored {
        return Ok(());
    }
    let keys = sent.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    let part = keys.iter().find(|k| sent[k.as_str()] != restored[k.as_str()]).cloned().unwrap_or_default();
    Err(format!("the restored state differs from the snapshot in {part}"))
}

#[cfg(test)]
mod tests {
    use crate::game::{test_game, tests::{join, save}};

    #[test]
    fn a_snapshot_restores_only_in_its_format_and_on_its_tables() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        let text = super::snapshot(&g).unwrap();
        let dir = crate::test_data().unwrap();
        let tables = || {
            let (n, d, s) = crate::tables(dir);
            let scenes = crate::scene_tables(dir, &n, &d);
            (n, d, s, scenes)
        };
        let (n, d, s, scenes) = tables();
        let back = super::restore(n, d, s, &text, scenes).unwrap();
        assert_eq!(back.player_object(1), Some(player));
        super::verify(&back, text.as_bytes()).unwrap();
        let (n, d, s, scenes) = tables();
        let mut other = super::restore(n, d, s, &text, scenes).unwrap();
        other.tmp_inventory_id += 1;
        assert!(super::verify(&other, text.as_bytes()).unwrap_err().contains("tmp_inventory_id"));
        for (from, to) in [(&*format!("\"format\":{}", super::FORMAT), "\"format\":99"), (&*format!("\"game_version\":\"{}\"", g.net_data.game_version), "\"game_version\":\"0.1\"")] {
            let (n, d, s, scenes) = tables();
            assert!(super::restore(n, d, s, &text.replacen(from, to, 1), scenes).is_err(), "{to} is refused");
        }
    }

    /// A position that is not a number would be written as null and never read back: the
    /// snapshot refuses, naming the object.
    #[test]
    fn a_snapshot_never_writes_a_position_it_cannot_read_back() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        g.net.objects.get_mut(&player).unwrap().position[1] = f32::NAN;
        assert!(super::snapshot(&g).unwrap_err().contains(&format!("network object {player}")));
        g.net.objects.get_mut(&player).unwrap().position[1] = 3.0;
        let pod = *g.wos.keys().next().unwrap();
        g.wos.get_mut(&pod).unwrap().rotation[0] = f32::INFINITY;
        assert!(super::snapshot(&g).unwrap_err().contains(&format!("world object {pod}")));
    }

    /// A snapshot as the release before wrote it (tests/fixtures/snapshot-<release>.json.gz: a
    /// world with one of every building and 4 players, made by tests/hot_update.rs
    /// make_the_snapshot_fixture_of_a_release with that release's program) restores in this
    /// build, goes on for a minute of game time, and is written and read again. 0.6.1 failed
    /// its first hot update on a field ("water_only") that 0.6.0's snapshot did not have, which
    /// no test found: they all restored what the same build had written. A release that no
    /// longer reads a fixture's format (FORMAT bumped twice) refuses it with that said.
    #[test]
    fn the_snapshot_of_every_release_restores() {
        let Some((net_data, data, spec, scenes)) = crate::game::test_tables() else { return };
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut fixtures: Vec<_> = std::fs::read_dir(&folder).unwrap().flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("snapshot-") && n.to_string_lossy().ends_with(".json.gz"))).collect();
        fixtures.sort();
        assert!(fixtures.len() >= 2, "a fixture per release since snapshots: {fixtures:?}");
        for path in fixtures {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let unzipped = std::process::Command::new("gzip").arg("-dc").arg(&path).output().unwrap();
            assert!(unzipped.status.success(), "{name}");
            let (head, _) = super::split(&unzipped.stdout).unwrap();
            if head.format + 1 < super::FORMAT {
                let e = super::restore_bytes(net_data.clone(), data.clone(), spec.clone(), &unzipped.stdout, scenes.clone()).map(|_| ()).unwrap_err();
                assert!(e.contains(&format!("format {}", head.format)) && e.contains("reads"), "{name}: {e}");
                continue;
            }
            let mut game = super::restore_bytes(net_data.clone(), data.clone(), spec.clone(), &unzipped.stdout, scenes.clone()).unwrap_or_else(|e| panic!("{name}: the release before's world does not restore in this build: {e}"));
            // Every building the game has was in it (the fixture is regenerated with a release
            // that has more of them, never with fewer).
            let placed: std::collections::HashSet<&str> = game.wos.values().map(|w| game.data.groups[w.group].id.as_str()).collect();
            // (A world does not keep an instance generator: it is the game's own object, made when a portal opens.)
            let missing: Vec<&str> = game.data.groups.iter().filter(|g| g.constructible && g.id != "ProceduralInstanceGenerator" && !placed.contains(g.id.as_str())).map(|g| g.id.as_str()).collect();
            assert!(missing.is_empty(), "{name}: no {missing:?} in the fixture");
            assert!(crate::machines::machine_count(&game) > 250 && game.clients.len() >= 4, "{name}: {} machines, {} clients", crate::machines::machine_count(&game), game.clients.len());
            // It goes on: a minute of game time on the clock of the snapshot.
            let start = f64::from(game.time);
            for second in 1..=60 {
                game.sim_time = Some(start + f64::from(second));
                game.tick();
            }
            // And is written and read again, as the next flip would.
            let again = super::snapshot(&game).unwrap();
            let back = super::restore_bytes(net_data.clone(), data.clone(), spec.clone(), again.as_bytes(), scenes.clone()).unwrap_or_else(|e| panic!("{name}: after a minute: {e}"));
            super::verify(&back, again.as_bytes()).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}
