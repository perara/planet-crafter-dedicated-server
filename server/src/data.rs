//! Static per-game-version tables (data/<version>/netdata.json, built by tools/extract_netdata.py)
//! and the game's string hash.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetData {
    pub game_version: String,
    pub tick_rate: u32,
    pub scene_handle: i32,
    /// (type-name hash, version) of every Netcode message, in the server's id order.
    pub message_versions: Vec<(u32, i32)>,
    pub scene_objects: Vec<ObjectDef>,
    pub prefabs: HashMap<String, ObjectDef>,
    pub planets: Vec<PlanetDef>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDef {
    #[serde(default)]
    pub id: u64,
    pub hash: u32,
    pub flags: u16,
    pub parent: Option<u64>,
    pub behaviours: Vec<BehaviourDef>,
    /// Custom synchronization blocks after the variables (e.g. NetworkTransform state).
    #[serde(default)]
    pub sync_tail: String,
    #[serde(default)]
    pub scale: Option<[f32; 3]>,
    /// Complete sync data of a freshly spawned instance (player objects).
    #[serde(default)]
    pub spawn_sync: String,
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
    /// Value observed when the table was extracted (hex); the default for this variable.
    pub bytes: String,
}

#[derive(Deserialize)]
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
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut h1: i32 = 5381;
    let mut h2: i32 = h1;
    let mut i = 0;
    while i < units.len() && units[i] != 0 {
        h1 = (h1.wrapping_shl(5).wrapping_add(h1)) ^ units[i] as i32;
        if i == units.len() - 1 || units[i + 1] == 0 {
            break;
        }
        h2 = (h2.wrapping_shl(5).wrapping_add(h2)) ^ units[i + 1] as i32;
        i += 2;
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
