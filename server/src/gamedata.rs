//! The game's static data for one version (data/<version>/gamedata.json, dumped from the game by
//! the plugin's GET /api/gamedata): groups (items and buildings) with their parameters, every
//! other game asset, and the network layout of each world object the server can spawn.

use std::collections::HashMap;

use serde_json::Value;

use crate::data::{stable_hash, BehaviourDef, VarDef};
use crate::protocol::read_scene_object;
use crate::wire::Reader;
use crate::world::hex;

/// DataConfig.WorldUnitType, in the game's order.
pub const UNIT_TYPES: [&str; 8] = ["Oxygen", "Pressure", "Heat", "Energy", "Plants", "Insects", "Animals", "Purification"];

pub struct Group {
    pub id: String,
    pub hash: i32,
    /// GroupDataConstructible (buildings) or GroupDataItem.
    pub constructible: bool,
    /// Ingredient group indices.
    pub recipe: Vec<usize>,
    pub inventory_size: i32,
    pub secondary_inventories: Vec<i32>,
    /// Per unit type: generation (buildings) or multiplier (items).
    pub units: [f32; 8],
    pub has_prefab: bool,
    /// The asset's serialized fields, for everything not lifted into typed fields.
    pub fields: Value,
}

impl Group {
    pub fn str(&self, key: &str) -> &str {
        self.fields[key].as_str().unwrap_or("")
    }
    pub fn f32(&self, key: &str) -> f32 {
        self.fields[key].as_f64().unwrap_or(0.0) as f32
    }
    pub fn bool(&self, key: &str) -> bool {
        self.fields[key].as_bool().unwrap_or(false)
    }
    pub fn usable_type(&self) -> &str {
        self.str("usableType")
    }
    pub fn unit(&self, unit: &str) -> f32 {
        UNIT_TYPES.iter().position(|u| *u == unit).map(|i| self.units[i]).unwrap_or(0.0)
    }
}

/// What a client receives for one spawned world object: prefab hash, flags, behaviours with
/// their variables (defaults), and the custom sync data after the variables.
#[derive(Clone)]
pub struct Layout {
    pub prefab: String,
    pub hash: u32,
    pub flags: u16,
    pub scale: [f32; 3],
    pub behaviours: Vec<BehaviourDef>,
    pub tail: Vec<u8>,
    /// Offset in `tail` of the transform state's position (the dump spawned at 0,-5000,0).
    pub tail_position: Option<usize>,
}

pub struct GameData {
    pub version: String,
    pub groups: Vec<Group>,
    by_id: HashMap<String, usize>,
    by_hash: HashMap<i32, usize>,
    /// Keyed by group id; "<id>#static" is the container variant for objects loaded from the save.
    pub layouts: HashMap<String, Layout>,
    /// Every game asset, keyed "Type:name".
    pub assets: serde_json::Map<String, Value>,
    pub group_prefabs: Value,
    /// Habitat panels: per group, each Panel's transform, colliders and bounds; the bounds of
    /// every panel look (panels.rs).
    pub panels: Value,
    pub singletons: Value,
    /// World objects that come with the dumped planet's scene.
    pub scene_world_objects: Vec<Value>,
}

const DUMP_POSITION: [u8; 12] = [0, 0, 0, 0, 0x00, 0x40, 0x9c, 0xc5, 0, 0, 0, 0];

fn layout(entry: &Value) -> Option<Layout> {
    let bytes = hex(entry["sceneObject"].as_str()?);
    let o = read_scene_object(&mut Reader::new(&bytes))?;
    let mut r = Reader::new(&bytes);
    r.skip(2 + 4)?;
    r.packed_u64()?;
    r.packed_u64()?;
    let behaviours: Vec<BehaviourDef> = entry["behaviours"]
        .as_array()?
        .iter()
        .map(|b| BehaviourDef {
            class: b["class"].as_str().unwrap_or("").rsplit('.').next().unwrap_or("").to_string(),
            behaviour_id: b["behaviourId"].as_u64().unwrap_or(0) as u16,
            vars: b["variables"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| VarDef { name: v["name"].as_str().unwrap_or("").into(), ty: v["type"].as_str().unwrap_or("").into(), bytes: v["bytes"].as_str().unwrap_or("").into() })
                .collect(),
        })
        .collect();
    let vars_len: usize = behaviours.iter().flat_map(|b| &b.vars).map(|v| v.bytes.len() / 2).sum();
    let tail = o.sync.get(vars_len..)?.to_vec();
    let tail_position = tail.windows(12).position(|w| w == DUMP_POSITION);
    // Scale follows position (12) and rotation (16) in the transform.
    let at = bytes.len() - o.sync.len() - 8 - 12;
    let scale = [0, 4, 8].map(|i| f32::from_le_bytes(bytes[at + i..at + i + 4].try_into().unwrap()));
    Some(Layout { prefab: entry["prefab"].as_str()?.into(), hash: o.hash, flags: o.flags, scale, behaviours, tail, tail_position })
}

impl GameData {
    pub fn load(path: &str) -> Result<GameData, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut root: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        let assets = root["assets"].as_object().cloned().unwrap_or_default();
        // Group assets are referenced as "Type:name"; their id is the "id" field.
        let mut ref_to_id: HashMap<&str, &str> = HashMap::new();
        for (key, fields) in &assets {
            if key.starts_with("GroupData") {
                if let Some(id) = fields["id"].as_str() {
                    ref_to_id.insert(key, id);
                }
            }
        }
        let mut entries: Vec<(&String, &Value)> = assets.iter().filter(|(k, _)| k.starts_with("GroupDataItem:") || k.starts_with("GroupDataConstructible:")).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut groups = Vec::new();
        for (key, fields) in entries {
            let Some(id) = fields["id"].as_str() else { continue };
            let constructible = key.starts_with("GroupDataConstructible:");
            let prefix = if constructible { "unitGeneration" } else { "unitMultiplier" };
            groups.push(Group {
                id: id.to_string(),
                hash: stable_hash(id),
                constructible,
                recipe: Vec::new(),
                inventory_size: fields["inventorySize"].as_i64().unwrap_or(0) as i32,
                secondary_inventories: fields["secondaryInventoriesSize"].as_array().into_iter().flatten().filter_map(Value::as_i64).map(|v| v as i32).collect(),
                units: UNIT_TYPES.map(|u| fields[format!("{prefix}{u}")].as_f64().unwrap_or(0.0) as f32),
                has_prefab: fields["associatedGameObject"].is_string(),
                fields: fields.clone(),
            });
        }
        let by_id: HashMap<String, usize> = groups.iter().enumerate().map(|(i, g)| (g.id.clone(), i)).collect();
        for g in &mut groups {
            g.recipe = g.fields["recipeIngredients"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| ref_to_id.get(r.as_str()?).and_then(|id| by_id.get(*id)).copied())
                .collect();
        }
        let by_hash = groups.iter().enumerate().map(|(i, g)| (g.hash, i)).collect();
        let layouts = root["layouts"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), layout(v)?))).collect();
        Ok(GameData {
            version: root["gameVersion"].as_str().unwrap_or("").into(),
            groups,
            by_id,
            by_hash,
            layouts,
            assets,
            group_prefabs: root["groupPrefabs"].take(),
            panels: root["panels"].take(),
            singletons: root["singletons"].take(),
            scene_world_objects: root["sceneWorldObjects"].as_array().cloned().unwrap_or_default(),
        })
    }

    pub fn group(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    pub fn group_by_hash(&self, hash: i32) -> Option<usize> {
        self.by_hash.get(&hash).copied()
    }

    /// Group id of an asset reference ("GroupDataItem:Iron").
    pub fn group_of_ref(&self, reference: &str) -> Option<usize> {
        self.group(self.assets.get(reference)?["id"].as_str()?)
    }

    /// The asset an asset reference points to.
    pub fn asset(&self, reference: &str) -> Option<&Value> {
        self.assets.get(reference)
    }

    /// Components of a group's prefab with a given type (a group's machine parameters).
    pub fn prefab_components(&self, group: &str, ty: &str) -> Vec<&Value> {
        self.group_prefabs[group]["components"].as_array().into_iter().flatten().filter(|c| c["type"] == ty).map(|c| &c["fields"]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_groups_and_layouts() {
        let Some(dir) = crate::test_data() else { return };
        let d = GameData::load(&format!("{dir}/gamedata.json")).unwrap();
        let iron = &d.groups[d.group("Iron").unwrap()];
        assert!(!iron.constructible && iron.has_prefab);
        let air = &d.groups[d.group("AirPurificationMachine1").unwrap()];
        assert_eq!(air.unit("Purification"), 7.0);
        assert_eq!(air.recipe.len(), 6);
        let container = &d.layouts["Container1"];
        assert_eq!(container.behaviours[0].class, "ConstructibleProxy");
        let dropped = &d.layouts["Iron"];
        assert!(dropped.tail_position.is_some(), "dynamic containers carry their transform state");
        assert!(d.layouts["Iron#static"].tail_position.is_none());
    }
}
