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
    /// Every game asset, keyed "Type:name" (a group's `fields` are its asset, shared).
    pub assets: Arc<indexmap::IndexMap<String, Arc<Value>>>,
    pub group_prefabs: Arc<Prefabs>,
    /// Habitat panels: per group, each Panel's transform, colliders and bounds; the bounds of
    /// every panel look (panels.rs).
    pub panels: Arc<Value>,
    pub singletons: Arc<Value>,
}

/// NetworkTransformState's position bits (x, y, z).
const POSITION_BITS: u32 = 0b1110;
/// UseUnreliableDeltas and UnreliableFrameSync: set on the states a moving owner sends between
/// its reliable ones. The host's synchronization state of an object that has not moved since it
/// spawned has neither (the real host sends the same flags as for every other object), but the
/// extracted state of a moving vehicle (the rover) carries them.
const UNRELIABLE_BITS: u32 = 3 << 20;

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
            state.packed_u32(t.flags & !UNRELIABLE_BITS).packed_i32(tick);
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

/// groupPrefabs by group id. A prefab is 1.8 MB of JSON for all 727 and 18 MiB as parsed
/// values, and a world reads those of the buildings it has: each is parsed the first time it
/// is read (`prefabs[group]` is a Value, Null for a group without one).
#[derive(Default, Clone)]
pub struct Prefabs(HashMap<String, Prefab>);

/// A prefab searched this often is parsed instead (the search is for the few asks of a start).
const MENTIONS_BEFORE_PARSING: u32 = 8;

struct Prefab {
    raw: Box<serde_json::value::RawValue>,
    parsed: std::sync::OnceLock<Value>,
    /// How often `mentions` looked through the text: a prefab asked about again and again is
    /// parsed, which is cheaper than searching it each time.
    asked: std::sync::atomic::AtomicU32,
    /// The type of each component and whether it is on the root (its path is empty), by place in
    /// the prefab's list: what a search for a type goes through, without a lookup of the JSON.
    kinds: std::sync::OnceLock<Vec<(Option<Box<str>>, bool)>>,
}

impl Clone for Prefab {
    fn clone(&self) -> Prefab {
        Prefab { raw: self.raw.clone(), parsed: self.parsed.clone(), asked: Default::default(), kinds: self.kinds.clone() }
    }
}

impl Prefab {
    fn value(&self) -> &Value {
        self.parsed.get_or_init(|| serde_json::from_str(self.raw.get()).unwrap_or(Value::Null))
    }

    fn kinds(&self) -> &[(Option<Box<str>>, bool)] {
        self.kinds.get_or_init(|| self.value()["components"].as_array().into_iter().flatten().map(|c| (c["type"].as_str().map(Box::from), c["path"] == "")).collect())
    }
}

impl Prefabs {
    /// Whether the prefab can hold `word` (a component type): false only when its text has no
    /// such word, so that asking does not parse the prefabs that cannot answer.
    pub fn mentions(&self, group: &str, word: &str) -> bool {
        self.0.get(group).is_some_and(|p| p.parsed.get().is_some() || p.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MENTIONS_BEFORE_PARSING || p.raw.get().contains(word))
    }

    /// The serialized fields of a group's prefab components of type `ty`, in the prefab's order
    /// (`root_only`: those on the root object). Empty for a group without a prefab.
    pub fn components(&self, group: &str, ty: &str, root_only: bool) -> Vec<&Value> {
        let Some(p) = self.0.get(group) else { return Vec::new() };
        if !self.mentions(group, ty) {
            return Vec::new();
        }
        let list = p.value()["components"].as_array().map(Vec::as_slice).unwrap_or_default();
        p.kinds().iter().zip(list).filter(|((t, root), _)| t.as_deref() == Some(ty) && (!root_only || *root)).map(|(_, c)| &c["fields"]).collect()
    }

    /// The types of a group's prefab components on the root object, in the prefab's order.
    pub fn root_types(&self, group: &str) -> impl Iterator<Item = &str> {
        self.0.get(group).into_iter().flat_map(|p| p.kinds().iter()).filter(|(_, root)| *root).filter_map(|(t, _)| t.as_deref())
    }

    /// Whether any prefab has a component of type `ty`: only the prefabs whose text has the word
    /// are parsed to see.
    pub fn any_component_of_type(&self, ty: &str) -> bool {
        self.0.values().filter(|p| p.parsed.get().is_some() || p.raw.get().contains(ty)).any(|p| p.value()["components"].as_array().into_iter().flatten().any(|c| c["type"] == ty))
    }

    /// (parsed, all): how many prefabs have been read.
    #[cfg(test)]
    pub fn parsed(&self) -> (usize, usize) {
        (self.0.values().filter(|p| p.parsed.get().is_some()).count(), self.0.len())
    }
}

impl std::ops::Index<&str> for Prefabs {
    type Output = Value;
    fn index(&self, group: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.0.get(group).map_or(&NULL, Prefab::value)
    }
}

impl std::ops::IndexMut<&str> for Prefabs {
    fn index_mut(&mut self, group: &str) -> &mut Value {
        let prefab = self.0.entry(group.to_string()).or_insert_with(|| Prefab { raw: serde_json::value::RawValue::NULL.to_owned(), parsed: Default::default(), asked: Default::default(), kinds: Default::default() });
        prefab.value();
        // What is handed out may be changed: the list of kinds is made again from it.
        prefab.kinds = Default::default();
        prefab.parsed.get_mut().expect("parsed above")
    }
}

/// gamedata.json as it is read: the parts that are used whole parsed, the prefabs left as text.
#[derive(serde::Deserialize)]
struct Root {
    #[serde(default)]
    assets: indexmap::IndexMap<String, Value>,
    #[serde(rename = "groupPrefabs", default)]
    group_prefabs: HashMap<String, Box<serde_json::value::RawValue>>,
    layouts: Box<serde_json::value::RawValue>,
    #[serde(default)]
    singletons: Value,
    #[serde(default)]
    panels: Value,
}

impl GameData {
    pub fn load(path: &str) -> Result<GameData, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let root: Root = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        let assets: indexmap::IndexMap<String, Arc<Value>> = root.assets.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        // Group assets are referenced as "Type:name"; their id is the "id" field.
        let mut ref_to_id: HashMap<&str, &str> = HashMap::new();
        for (key, fields) in &assets {
            if key.starts_with("GroupData") {
                if let Some(id) = fields["id"].as_str() {
                    ref_to_id.insert(key, id);
                }
            }
        }
        let mut entries: Vec<(&String, &Arc<Value>)> = assets.iter().filter(|(k, _)| k.starts_with("GroupDataItem:") || k.starts_with("GroupDataConstructible:")).collect();
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
        let layouts = serde_json::from_str(root.layouts.get()).map_err(|e| format!("{path}: layouts: {e}"))?;
        Ok(GameData {
            groups,
            by_id,
            by_hash,
            layouts: Arc::new(layouts),
            assets: Arc::new(assets),
            group_prefabs: Arc::new(Prefabs(root.group_prefabs.into_iter().map(|(k, raw)| (k, Prefab { raw, parsed: Default::default(), asked: Default::default(), kinds: Default::default() })).collect())),
            panels: Arc::new(root.panels),
            singletons: Arc::new(root.singletons),
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
        self.assets.get(reference).map(|a| &**a)
    }

    /// Components of a group's prefab with a given type (a group's machine parameters).
    pub fn prefab_components(&self, group: &str, ty: &str) -> Vec<&Value> {
        self.group_prefabs.components(group, ty, false)
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

    /// The rover's extracted transform state was taken while it moved, with the unreliable-delta
    /// flags; its late-join state is the one of any resting object (the real host's).
    #[test]
    fn a_resting_vehicle_synchronizes_like_any_object() {
        let Some(dir) = crate::test_data() else { return };
        let d = GameData::load(&format!("{dir}/gamedata.json")).unwrap();
        let state = |group: &str| {
            let tail = d.layouts[group].layout(7).tail;
            // u8 count, then per state its behaviour (u16), length (i32) and the packed flags.
            let mut r = crate::wire::Reader::new(&tail[7..]);
            r.packed_u32().unwrap()
        };
        assert_eq!(state("VehicleTruck"), state("FloorAngleGlass"));
    }
}
