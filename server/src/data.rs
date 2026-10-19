//! Static per-game-version tables (data/<version>/netdata.json, read from the game's files by
//! `pcds read-game`) and the game's string hash.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NetData {
    pub game_version: String,
    pub tick_rate: u32,
    /// (type-name hash, version) of every Netcode message, in the server's id order.
    pub message_versions: Vec<(u32, i32)>,
    pub scene_objects: Vec<ObjectDef>,
    pub prefabs: HashMap<String, ObjectDef>,
    pub planets: Vec<PlanetDef>,
}

/// A network object as a spawn describes it (an in-scene object, a prefab, a world object's
/// layout).
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDef {
    /// In-scene objects: their network object id.
    #[serde(default)]
    pub id: u64,
    /// World object layouts: the prefab's name.
    #[serde(default)]
    pub prefab: String,
    pub hash: u32,
    pub flags: u16,
    pub parent: Option<u64>,
    pub behaviours: Vec<BehaviourDef>,
    /// Its NetworkTransforms' spawn states (NetworkTransform.OnSynchronize).
    pub transforms: Vec<TransformDef>,
    pub scale: Option<[f32; 3]>,
    /// Whether a world object of this prefab keeps its group hash at spawn (the root carries
    /// a GroupNetworkBase and has children).
    pub group_hash_on_spawn: bool,
}

#[derive(Deserialize, Clone)]
pub struct TransformDef {
    /// The NetworkBehaviour index.
    pub behaviour: u16,
    /// NetworkTransformState flags of a synchronization.
    pub flags: u32,
    /// On the object's root: its position is the object's.
    pub root: bool,
    /// The synchronized axes' values: positions, then angles.
    pub values: Vec<f32>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BehaviourDef {
    pub class: String,
    pub behaviour_id: u16,
    pub vars: Vec<VarDef>,
}

#[derive(Deserialize, Clone)]
pub struct VarDef {
    pub name: String,
    /// Netcode type, e.g. "NetworkVariable<Int32>".
    #[serde(rename = "type", default)]
    pub ty: String,
    /// The value it starts with (hex), as the game's constructor gives it; None when the server
    /// must set it (types with their own serializer).
    pub bytes: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanetDef {
    pub id: String,
    pub steam_app_id: u32,
    /// PlanetData.startingPlanet: a new world can start here.
    #[serde(default)]
    pub starting_planet: bool,
}

impl NetData {
    pub fn load(path: &str) -> Result<NetData, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    pub fn planet_index(&self, id: &str) -> Option<usize> {
        self.planets.iter().position(|p| p.id == id)
    }
}

/// StringExtensionMethods.GetStableHashCode: the .NET Framework string hash over UTF-16 units.
pub fn stable_hash(s: &str) -> i32 {
    let mut units = s.encode_utf16();
    let mut h1: i32 = 5381;
    let mut h2: i32 = h1;
    // Pairs of units up to a NUL, as the game reads the string (no Vec: this runs on hot paths).
    while let Some(a) = units.next().filter(|u| *u != 0) {
        h1 = (h1.wrapping_shl(5).wrapping_add(h1)) ^ a as i32;
        let Some(b) = units.next().filter(|u| *u != 0) else { break };
        h2 = (h2.wrapping_shl(5).wrapping_add(h2)) ^ b as i32;
    }
    h1.wrapping_add(h2.wrapping_mul(1566083941))
}

#[cfg(test)]
mod tests {
    use super::stable_hash;

    #[test]
    fn stable_hash_matches_the_game() {
        assert_eq!(stable_hash("Prime"), -1140328421);
        assert_eq!(stable_hash("Humble"), -486276833);
        assert_eq!(stable_hash("Container1"), 1768717124);
        assert_eq!(stable_hash("CraftStation1"), 769498401);
        assert_eq!(stable_hash("QuartzCraftStation1"), -1824094490);
    }
}
