//! The game's static data for one version (data/<version>/gamedata.json, read from the game's
//! files by `pcds read-game`): groups (items and buildings) with their parameters, every
//! other game asset, and the network layout of each world object the server can spawn.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::data::{stable_hash, BehaviourDef, ObjectDef};
use crate::wire::Writer;

/// DataConfig.WorldUnitType, in the game's order.
pub const UNIT_TYPES: [&str; 8] = ["Oxygen", "Pressure", "Heat", "Energy", "Plants", "Insects", "Animals", "Purification"];

#[derive(Clone)]
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
    pub fields: Arc<Value>,
}

impl Group {
    pub fn str(&self, key: &str) -> &str {
        self.fields[key].as_str().unwrap_or("")
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
    pub hash: u32,
    pub flags: u16,
    pub scale: [f32; 3],
    pub behaviours: Vec<BehaviourDef>,
    pub tail: Vec<u8>,
    /// Offset in `tail` of the root transform state's position, filled in at each send.
    pub tail_position: Option<usize>,
}

#[derive(Default, Clone)]
pub struct GameData {
    pub groups: Vec<Group>,
    by_id: HashMap<String, usize>,
    by_hash: HashMap<i32, usize>,
    /// Keyed by group id; "<id>#static" is the container variant for objects loaded from the save.
    /// Per group id (and "<id>#static"): the network object its world objects spawn as.
    pub layouts: Arc<HashMap<String, ObjectDef>>,
    /// Every game asset, keyed "Type:name".
    pub assets: Arc<serde_json::Map<String, Value>>,
    pub group_prefabs: Arc<Value>,
    /// Habitat panels: per group, each Panel's transform, colliders and bounds; the bounds of
    /// every panel look (panels.rs).
    pub panels: Arc<Value>,
    pub singletons: Arc<Value>,
}

/// NetworkTransformState's position bits (x, y, z).
const POSITION_BITS: u32 = 0b1110;

impl ObjectDef {
    /// The layout a spawn at `tick` sends: the custom sync data after the variables is each
    /// NetworkTransform's state (u8 count; per transform its behaviour, length, flags, tick and
    /// values), with the offset of the root's position for the spawn to fill in.
    pub fn layout(&self, tick: i32) -> Layout {
        let mut w = Writer::new();
        w.u8(self.transforms.len() as u8);
        let mut tail_position = None;
        for t in &self.transforms {
            let mut state = Writer::new();
            state.packed_u32(t.flags).packed_i32(tick);
            let at = state.len();
            t.values.iter().for_each(|v| {
                state.f32(*v);
            });
            w.u16(t.behaviour).i32(state.len() as i32);
            if t.root && t.flags & POSITION_BITS == POSITION_BITS && tail_position.is_none() {
                tail_position = Some(w.len() + at);
            }
            w.bytes(&state.0);
        }
        Layout { hash: self.hash, flags: self.flags, scale: self.scale.unwrap_or([1.0; 3]), behaviours: self.behaviours.clone(), tail: w.0, tail_position }
    }
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
                fields: Arc::new(fields.clone()),
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
        let layouts = serde_json::from_value(root["layouts"].take()).map_err(|e| format!("{path}: layouts: {e}"))?;
        Ok(GameData {
            groups,
            by_id,
            by_hash,
            layouts: Arc::new(layouts),
            assets: Arc::new(assets),
            group_prefabs: Arc::new(root["groupPrefabs"].take()),
            panels: Arc::new(root["panels"].take()),
            singletons: Arc::new(root["singletons"].take()),
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
        let dropped = d.layouts["Iron"].layout(7);
        assert!(dropped.tail_position.is_some(), "dynamic containers carry their transform state");
        assert!(d.layouts["Iron#static"].layout(7).tail_position.is_none());
    }
}
