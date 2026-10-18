//! Hot-update spike: the game's whole runtime state as one JSON document, and a game rebuilt
//! from it that goes on exactly as the old one would have (see docs/hot-update.md). The static
//! tables (data/<version>/) are not in it: the new process loads its own.

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
pub const FORMAT: u32 = 1;

/// The snapshot's first line: its format, and the tables it is for (state holds indices into
/// them, so it is only valid on the same game version).
#[derive(serde::Serialize, serde::Deserialize)]
struct Head {
    format: u32,
    game_version: String,
}

/// The game's state at a tick boundary (main sent everything queued: net.out is empty). An
/// error leaves the game as it was: it goes on, and the handoff is off.
pub fn snapshot(g: &Game) -> Result<String, String> {
    let head = Head { format: FORMAT, game_version: g.net_data.game_version.clone() };
    let (head, state) = (serde_json::to_string(&head), serde_json::to_string(g));
    Ok(format!("{}\n{}", head.map_err(|e| e.to_string())?, state.map_err(|e| e.to_string())?))
}

/// The previous format's state brought to this one, as firmware migrates its data on boot.
fn migrate(from: u32, state: serde_json::Value) -> Result<serde_json::Value, String> {
    match from {
        // FORMAT - 1 => { state["ext"]["meteo"]["renamed"] = state["ext"]["meteo"]["old"].take(); Ok(state) }
        _ => {
            let _ = state;
            Err(format!("snapshot format {from}: this build reads format {FORMAT} and the one before"))
        }
    }
}

/// A game from a snapshot and this process's own tables.
pub fn restore(net_data: NetData, data: GameData, spec: Spec, snapshot: &str, scenes: crate::Scenes) -> Result<Game, String> {
    let (head, state) = snapshot.split_once('\n').ok_or("not a snapshot")?;
    let head: Head = serde_json::from_str(head).map_err(|e| e.to_string())?;
    if head.game_version != net_data.game_version {
        return Err(format!("a snapshot on game {}'s tables, these are {}'s", head.game_version, net_data.game_version));
    }
    let mut g: Game = match head.format {
        FORMAT => serde_json::from_str(state).map_err(|e| e.to_string())?,
        f => serde_json::from_value(migrate(f, serde_json::from_str(state).map_err(|e| e.to_string())?)?).map_err(|e| e.to_string())?,
    };
    g.net_data = net_data;
    g.data = data;
    g.net.spec = spec;
    g.scenes = scenes;
    g.trace = std::env::var_os("PCDS_TRACE").is_some();
    // The tables are not quite static: Game::new scales the groups' generation by the
    // world's settings.
    g.change_generation_for_groups();
    Ok(g)
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
        for (from, to) in [("\"format\":1", "\"format\":99"), (&*format!("\"game_version\":\"{}\"", g.net_data.game_version), "\"game_version\":\"0.1\"")] {
            let (n, d, s, scenes) = tables();
            assert!(super::restore(n, d, s, &text.replacen(from, to, 1), scenes).is_err(), "{to} is refused");
        }
    }
}
