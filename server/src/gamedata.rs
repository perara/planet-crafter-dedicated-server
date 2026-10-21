//! The game's static data for one version (data/<version>/gamedata.json, read from the game's
//! files by `pcds read-game`): groups (items and buildings) with their parameters, every
//! other game asset, and the network layout of each world object the server can spawn.

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
use serde_json::Value;

use crate::doc::Doc;

use crate::data::{stable_hash, BehaviourDef, Compiled, ObjectDef};
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
    pub fields: Arc<Doc>,
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
    /// The behaviours' shape and starting values.
    pub compiled: Arc<Compiled>,
    pub tail: Vec<u8>,
    /// Offset in `tail` of the root transform state's position, filled in at each send.
    pub tail_position: Option<usize>,
}

impl Layout {
    /// A layout made on the spot (a generator, an animal) from its behaviours.
    pub fn new(hash: u32, flags: u16, behaviours: &[BehaviourDef], tail: Vec<u8>, tail_position: Option<usize>) -> Layout {
        Layout { hash, flags, scale: [1.0; 3], compiled: Arc::new(Compiled::of(behaviours)), tail, tail_position }
    }
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
    pub assets: Arc<indexmap::IndexMap<String, Arc<Doc>>>,
    pub group_prefabs: Arc<Prefabs>,
    /// Habitat panels: per group, each Panel's transform, colliders and bounds; the bounds of
    /// every panel look (panels.rs).
    pub panels: Arc<Doc>,
    pub singletons: Arc<Doc>,
    /// The collision map for drones.avoidObstacles, read from its file when something first needs it.
    pub drone_navigation: Arc<DroneMap>,
}

/// The optional drone collision map (`pcds drone-map`, geometry::navigation) beside the tables. It
/// is 55 MB of JSON for the six planets and 18 MB parsed, so it is read, checked against the
/// tables' game build and kept only when a rule or the drones first need it: a server that has the
/// file and does not use the rule pays nothing for it.
#[derive(Default)]
pub struct DroneMap {
    /// The map's file and the tables' `buildid` it must match; None: there is no map.
    files: Option<(std::path::PathBuf, std::path::PathBuf)>,
    data: std::sync::OnceLock<Result<Arc<crate::geometry::navigation::Data>, String>>,
}

impl DroneMap {
    /// The map next to `path`, if the file is there (an unreadable one is an error, a missing one is none).
    fn beside(path: &std::path::Path) -> Result<DroneMap, String> {
        match std::fs::metadata(path) {
            Ok(_) => Ok(DroneMap { files: Some((path.to_path_buf(), path.with_file_name("buildid"))), data: Default::default() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DroneMap::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// A map that is already in memory (tests).
    #[cfg(test)]
    pub fn of(map: crate::geometry::navigation::Data) -> DroneMap {
        DroneMap { files: None, data: std::sync::OnceLock::from(Ok(Arc::new(map))) }
    }

    /// The map, read and checked the first time (the answer, error included, is kept).
    pub fn get(&self) -> Result<Arc<crate::geometry::navigation::Data>, String> {
        self.data
            .get_or_init(|| {
                let Some((file, build)) = &self.files else {
                    return Err("the game tables have no collision map (drone-navigation.json); make it with `pcds drone-map GAME_DIR TABLES_DIR`".into());
                };
                let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
                let map: crate::geometry::navigation::Data = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
                let build = std::fs::read_to_string(build).map_err(|e| format!("collision map buildid: {e}"))?;
                map.validate(&build)?;
                Ok(Arc::new(map))
            })
            .clone()
    }
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
        Layout { hash: self.hash, flags: self.flags, scale: self.scale.unwrap_or([1.0; 3]), compiled: self.compiled(), tail: w.0, tail_position }
    }
}

/// groupPrefabs by group id. A prefab is 1.8 MB of JSON for all 727 and 18 MiB as parsed
/// values, and a world reads those of the buildings it has: each is parsed the first time it
/// is read (`prefabs[group]` is a Doc, Null for a group without one).
#[derive(Default, Clone)]
pub struct Prefabs(HashMap<String, Prefab>);

/// A prefab searched this often is parsed instead (the search is for the few asks of a start).
const MENTIONS_BEFORE_PARSING: u32 = 8;

struct Prefab {
    /// The prefab's JSON without the whitespace of the file it was read from (which is indented).
    raw: Box<str>,
    parsed: std::sync::OnceLock<Doc>,
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
    fn value(&self) -> &Doc {
        self.parsed.get_or_init(|| serde_json::from_str(&self.raw).unwrap_or(Doc::Null))
    }

    fn kinds(&self) -> &[(Option<Box<str>>, bool)] {
        self.kinds.get_or_init(|| self.value()["components"].as_array().into_iter().flatten().map(|c| (c["type"].as_str().map(Box::from), c["path"] == "")).collect())
    }
}

impl Prefabs {
    /// Whether the prefab can hold `word` (a component type): false only when its text has no
    /// such word, so that asking does not parse the prefabs that cannot answer.
    pub fn mentions(&self, group: &str, word: &str) -> bool {
        self.0.get(group).is_some_and(|p| p.parsed.get().is_some() || p.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MENTIONS_BEFORE_PARSING || p.raw.contains(word))
    }

    /// The serialized fields of a group's prefab components of type `ty`, in the prefab's order
    /// (`root_only`: those on the root object). Empty for a group without a prefab.
    pub fn components(&self, group: &str, ty: &str, root_only: bool) -> Vec<&Doc> {
        let Some(p) = self.0.get(group) else { return Vec::new() };
        if !self.mentions(group, ty) {
            return Vec::new();
        }
        let list = p.value()["components"].as_array().unwrap_or_default();
        p.kinds().iter().zip(list).filter(|((t, root), _)| t.as_deref() == Some(ty) && (!root_only || *root)).map(|(_, c)| &c["fields"]).collect()
    }

    /// The types of a group's prefab components on the root object, in the prefab's order.
    pub fn root_types(&self, group: &str) -> impl Iterator<Item = &str> {
        self.0.get(group).into_iter().flat_map(|p| p.kinds().iter()).filter(|(_, root)| *root).filter_map(|(t, _)| t.as_deref())
    }

    /// Whether any prefab has a component of type `ty`: only the prefabs whose text has the word
    /// are parsed to see.
    pub fn any_component_of_type(&self, ty: &str) -> bool {
        self.0.values().filter(|p| p.parsed.get().is_some() || p.raw.contains(ty)).any(|p| p.value()["components"].as_array().into_iter().flatten().any(|c| c["type"] == ty))
    }

    /// (parsed, all): how many prefabs have been read.
    #[cfg(test)]
    pub fn parsed(&self) -> (usize, usize) {
        (self.0.values().filter(|p| p.parsed.get().is_some()).count(), self.0.len())
    }
}

impl std::ops::Index<&str> for Prefabs {
    type Output = Doc;
    fn index(&self, group: &str) -> &Doc {
        static NULL: Doc = Doc::Null;
        self.0.get(group).map_or(&NULL, Prefab::value)
    }
}

impl Prefabs {
    /// A prefab changed by `edit` (a test's way to give a building a component the tables lack): it
    /// is edited as a `Value` and read back as a document.
    #[cfg(test)]
    pub fn edit(&mut self, group: &str, edit: impl FnOnce(&mut Value)) {
        let prefab = self.0.entry(group.to_string()).or_insert_with(|| Prefab { raw: "null".into(), parsed: Default::default(), asked: Default::default(), kinds: Default::default() });
        let mut value = prefab.value().to_value();
        edit(&mut value);
        // What the prefab says is what the document says: the text it is kept as is made again, and the list of kinds.
        *prefab = Prefab { raw: serde_json::to_string(&value).unwrap_or_default().into(), parsed: Default::default(), asked: Default::default(), kinds: Default::default() };
    }
}

/// gamedata.json as it is read: the parts that are used whole parsed, the prefabs left as text.
#[derive(serde::Deserialize)]
struct Root {
    #[serde(default)]
    assets: indexmap::IndexMap<String, Doc>,
    #[serde(rename = "groupPrefabs", default)]
    group_prefabs: HashMap<String, Box<serde_json::value::RawValue>>,
    layouts: Box<serde_json::value::RawValue>,
    #[serde(default)]
    singletons: Doc,
    #[serde(default)]
    panels: Doc,
}

impl GameData {
    pub fn load(path: &str) -> Result<GameData, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let root: Root = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        let assets: indexmap::IndexMap<String, Arc<Doc>> = root.assets.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        // Group assets are referenced as "Type:name"; their id is the "id" field.
        let mut ref_to_id: HashMap<&str, &str> = HashMap::new();
        for (key, fields) in &assets {
            if key.starts_with("GroupData") {
                if let Some(id) = fields["id"].as_str() {
                    ref_to_id.insert(key, id);
                }
            }
        }
        let mut entries: Vec<(&String, &Arc<Doc>)> = assets.iter().filter(|(k, _)| k.starts_with("GroupDataItem:") || k.starts_with("GroupDataConstructible:")).collect();
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
                secondary_inventories: fields["secondaryInventoriesSize"].as_array().into_iter().flatten().filter_map(Doc::as_i64).map(|v| v as i32).collect(),
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
        let map_path = std::path::Path::new(path).with_file_name("drone-navigation.json");
        let drone_navigation = Arc::new(DroneMap::beside(&map_path)?);
        Ok(GameData {
            groups,
            by_id,
            by_hash,
            layouts: Arc::new(layouts),
            assets: Arc::new(assets),
            group_prefabs: Arc::new(Prefabs(root.group_prefabs.into_iter().map(|(k, raw)| (k, Prefab { raw: crate::util::compact_json(raw.get()), parsed: Default::default(), asked: Default::default(), kinds: Default::default() })).collect())),
            panels: Arc::new(root.panels),
            singletons: Arc::new(root.singletons),
            drone_navigation,
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
    pub fn asset(&self, reference: &str) -> Option<&Doc> {
        self.assets.get(reference).map(|a| &**a)
    }

    /// Components of a group's prefab with a given type (a group's machine parameters).
    pub fn prefab_components(&self, group: &str, ty: &str) -> Vec<&Doc> {
        self.group_prefabs.components(group, ty, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prefab kept as text without its indentation is the same document: strings keep their
    /// spaces, escaped quotes and backslashes, and what comes back is the compact form serde_json writes.
    #[test]
    fn compact_text_is_the_same_document_without_its_indentation() {
        let doc = serde_json::json!({"components": [{"type": "A b", "fields": {"s": "a \"quoted\" \\ text\n\t with  spaces ", "n": [1, 2.5, -3e-7, null, true], "e": {}, "l": []}}], "k y": "\\\""});
        for text in [serde_json::to_string_pretty(&doc).unwrap(), serde_json::to_string(&doc).unwrap(), format!(" \r\n{}\t ", serde_json::to_string_pretty(&doc).unwrap())] {
            assert_eq!(&*crate::util::compact_json(&text), serde_json::to_string(&doc).unwrap());
        }
    }

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
    #[test]
    fn optional_drone_maps_are_read_when_needed_and_only_for_matching_tables() {
        let dir = std::env::temp_dir().join(format!("pcds-drone-map-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("gamedata.json");
        std::fs::write(&path, r#"{"layouts":{}}"#).unwrap();
        let load = || GameData::load(path.to_str().unwrap()).unwrap();
        assert!(load().drone_navigation.get().is_err_and(|e| e.contains("pcds drone-map")), "no file, no map");
        std::fs::write(dir.join("buildid"), "abc").unwrap();
        std::fs::write(dir.join("drone-navigation.json"), r#"{"format":1,"build_id":"abc","planets":{},"buildings":{}}"#).unwrap();
        assert!(load().drone_navigation.get().is_ok());
        std::fs::write(dir.join("buildid"), "different").unwrap();
        let stale = load();
        assert!(stale.drone_navigation.get().is_err_and(|e| e.contains("pcds drone-map")), "the map of another build");
        std::fs::write(dir.join("buildid"), "abc").unwrap();
        std::fs::write(dir.join("drone-navigation.json"), "bad json").unwrap();
        let bad = load();
        assert!(bad.drone_navigation.get().is_err(), "a malformed map");
        // The start does not read the file: a server that does not use the rule never pays for it.
        std::fs::write(dir.join("drone-navigation.json"), r#"{"format":1,"build_id":"abc","planets":{},"buildings":{}}"#).unwrap();
        assert!(bad.drone_navigation.get().is_err(), "the answer is kept");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cached_components_keep_order_root_filter_and_edits() {
        let Some(dir) = crate::test_data() else { return };
        let text = std::fs::read_to_string(format!("{dir}/gamedata.json")).unwrap();
        let raw: Value = serde_json::from_str(&text).unwrap();
        let mut data = GameData::load(&format!("{dir}/gamedata.json")).unwrap();
        for (group, prefab) in raw["groupPrefabs"].as_object().unwrap() {
            let components: Vec<_> = prefab["components"].as_array().into_iter().flatten().collect();
            let types: std::collections::HashSet<_> = components.iter().filter_map(|c| c["type"].as_str()).chain(["NoSuchComponent"]).collect();
            for ty in types {
                for root in [false, true] {
                    let expected: Vec<_> = components.iter().filter(|c| c["type"] == ty && (!root || c["path"] == "")).map(|c| c["fields"].clone()).collect();
                    let actual: Vec<_> = data.group_prefabs.components(group, ty, root).iter().map(|c| c.to_value()).collect();
                    assert_eq!(actual, expected, "{group}: {ty}, root={root}");
                }
            }
        }
        let prefabs = Arc::make_mut(&mut data.group_prefabs);
        prefabs.edit("CacheTest", |p| *p = serde_json::json!({"components":[{"type":"A","path":"","fields":{"n":1}},{"type":"A","path":"child","fields":{"n":2}},{"type":"A","path":"","fields":{"n":3}}]}));
        assert_eq!(prefabs.components("CacheTest", "A", false).iter().map(|c| c["n"].as_i64()).collect::<Vec<_>>(), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(prefabs.components("CacheTest", "A", true).len(), 2);
        prefabs.edit("CacheTest", |p| p["components"][0]["type"] = serde_json::json!("B"));
        assert_eq!(prefabs.components("CacheTest", "A", true)[0]["n"].as_i64(), Some(3));
        assert_eq!(prefabs.components("CacheTest", "B", true)[0]["n"].as_i64(), Some(1));
    }
}
