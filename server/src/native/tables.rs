//! The game tables read from the game's own files, with no game running:
//! `pcds-server read-game GAME_DIR OUT_DIR` writes what `pcds extract` gets from the running
//! game (extractor/*.cs), table by table, so `tables-diff` can hold one against the other.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use serde_json::{json, Map, Value};

use super::dotnet::{Assemblies, TypeName};
use super::graph::Graph;
use super::serial::{enum_name, read_fields, Layouts, Raw};
use super::unity::{self, Data, Id, Reader};

/// GameData.MaxDepth.
const MAX_DEPTH: usize = 6;

/// A MonoBehaviour (or ScriptableObject) as read: its script, game object, name and fields.
pub struct Behaviour {
    pub namespace: String,
    pub class: String,
    pub game_object: Option<Id>,
    pub enabled: bool,
    pub name: String,
    pub fields: Vec<(String, Raw)>,
}

pub struct Ctx {
    pub data: Data,
    pub graph: Graph,
    pub layouts: Layouts,
    /// Behaviours whose fields did not end where their data ends (a layout to fix).
    pub misread: HashMap<String, usize>,
    /// Group id -> its prefab's root game object (GroupData.associatedGameObject).
    pub groups: Vec<(String, Option<Id>)>,
    /// Files whose game objects are in the graph.
    graphed: std::collections::HashSet<usize>,
}

/// The shortest decimal that reads back as this float (as the extractor's JSON writes floats).
pub fn single(f: f32) -> Value {
    format!("{f}").parse::<f64>().map(|v| json!(v)).unwrap_or(Value::Null)
}

impl Ctx {
    pub fn open(game: &Path) -> std::io::Result<Ctx> {
        let dir = game.join("Planet Crafter_Data");
        let data = Data::open(&dir)?;
        let asm = Assemblies::load_dir(&dir.join("Managed"))?;
        Ok(Ctx { data, graph: Graph::default(), layouts: Layouts::new(Rc::new(asm)), misread: HashMap::new(), groups: Vec::new(), graphed: Default::default() })
    }

    pub fn behaviour(&mut self, id: Id) -> Option<Behaviour> {
        let (namespace, class, assembly) = self.graph.script(&mut self.data, id)?.clone();
        let fields = self.layouts.fields(&TypeName { scope: Some(assembly.trim_end_matches(".dll").to_string()), namespace: namespace.clone(), name: class.clone() });
        let file = id.0;
        let mut r = self.data.reader(id)?;
        let head = unity::behaviour(&mut r)?;
        let values = read_fields(&fields, &mut r);
        if values.is_none() || r.pos != r.b.len() {
            *self.misread.entry(format!("{namespace}.{class}")).or_default() += 1;
        }
        let game_object = self.data.resolve(file, head.game_object);
        Some(Behaviour { namespace, class, game_object, enabled: head.enabled, name: head.name, fields: values.unwrap_or_default() })
    }

    fn game_object_name(&mut self, id: Id) -> Option<String> {
        let mut r = self.data.reader(id)?;
        unity::game_object(&mut r).map(|g| g.name)
    }

    /// GameData.Value of a reference: "Type:name" for game assets, "GameObject:name",
    /// "Type@gameObject" for components; None (left out) for other Unity objects.
    fn reference(&mut self, file: usize, p: unity::PPtr) -> Option<Value> {
        let Some(target) = self.data.resolve(file, p) else { return Some(Value::Null) };
        let Some(class) = self.data.class_of(target) else { return Some(Value::Null) };
        match class {
            unity::GAME_OBJECT => Some(json!(format!("GameObject:{}", self.game_object_name(target)?))),
            unity::MONO_BEHAVIOUR => {
                let mut r = self.data.reader(target)?;
                let head = unity::behaviour(&mut r)?;
                let (namespace, script, _) = self.graph.script(&mut self.data, target)?.clone();
                match self.data.resolve(target.0, head.game_object) {
                    None => (namespace == "SpaceCraft").then(|| json!(format!("{script}:{}", head.name))),
                    Some(go) => Some(json!(format!("{script}@{}", self.game_object_name(go)?))),
                }
            }
            _ => {
                // A component (its data starts with its game object), named by its class.
                let go = self.data.reader(target)?.pptr()?;
                let go = self.data.resolve(target.0, go).filter(|g| self.data.class_of(*g) == Some(unity::GAME_OBJECT))?;
                Some(json!(format!("{}@{}", unity::class_name(class)?, self.game_object_name(go)?)))
            }
        }
    }

    /// GameData.Value: the value as JSON, None when the extractor leaves the field out.
    pub fn value(&mut self, file: usize, v: &Raw, depth: usize) -> Option<Value> {
        match v {
            Raw::Null => Some(Value::Null),
            Raw::Bool(b) => Some(json!(b)),
            Raw::Int(i) => Some(json!(i)),
            Raw::UInt(u) => Some(json!(u)),
            Raw::Float(f) => Some(json!(f)),
            Raw::Single(f) => Some(single(*f)),
            Raw::Str(s) => Some(json!(s)),
            Raw::Enum(v, names, flags) => Some(json!(enum_name(*v, names, *flags))),
            Raw::Ref(p, _) => self.reference(file, *p),
            Raw::Json(j) => Some(j.clone()),
            Raw::Omit => None,
            Raw::List(items) => {
                if depth > MAX_DEPTH {
                    return None;
                }
                Some(Value::Array(items.iter().map(|i| self.value(file, i, depth + 1).unwrap_or(Value::Null)).collect()))
            }
            Raw::Fields(t, fields) => {
                if depth > MAX_DEPTH || t.namespace != "SpaceCraft" {
                    return None;
                }
                Some(Value::Object(self.fields(file, fields, depth)))
            }
        }
    }

    /// GameData.Fields: each field's value one level deeper; fields of derived classes win.
    pub fn fields(&mut self, file: usize, fields: &[(String, Raw)], depth: usize) -> Map<String, Value> {
        let mut out = Map::new();
        for (name, v) in fields.iter().rev() {
            if out.contains_key(name) {
                continue;
            }
            if let Some(j) = self.value(file, v, depth + 1) {
                out.insert(name.clone(), j);
            }
        }
        out
    }

    /// GameData.Assets: every game ScriptableObject, keyed "Type:name".
    pub fn assets(&mut self) -> Map<String, Value> {
        let mut names: Vec<String> = std::fs::read_dir(&self.data.dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".assets")).collect();
        names.sort();
        let mut found: Vec<(String, String, Value)> = Vec::new();
        for name in names {
            let Ok(f) = self.data.file(&name) else { continue };
            let behaviours: Vec<i64> = self.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
            for path in behaviours {
                let id = Id(f, path);
                // ScriptableObjects only: no game object. The script read first is cheap.
                let Some(head) = self.data.reader(id).and_then(|mut r| unity::behaviour(&mut r)) else { continue };
                if !head.game_object.is_null() || self.graph.script(&mut self.data, id).is_none_or(|s| s.0 != "SpaceCraft") {
                    continue;
                }
                let Some(b) = self.behaviour(id) else { continue };
                if b.class.starts_with("GroupData") {
                    let id_field = b.fields.iter().find_map(|(n, v)| match v {
                        Raw::Str(s) if n == "id" => Some(s.clone()),
                        _ => None,
                    });
                    let prefab = b.fields.iter().find_map(|(n, v)| match v {
                        Raw::Ref(p, _) if n == "associatedGameObject" => Some(*p),
                        _ => None,
                    });
                    if let Some(gid) = id_field {
                        let prefab = prefab.and_then(|p| self.data.resolve(f, p));
                        self.groups.push((gid, prefab));
                    }
                }
                let fields = Value::Object(self.fields(f, &b.fields, 0));
                found.push((format!("{}:{}", b.class, b.name), b.name, fields));
            }
        }
        // OrderBy(name), later ones replacing earlier ones under the same key.
        found.sort_by(|a, b| a.1.cmp(&b.1));
        let mut assets = Map::new();
        for (key, _, fields) in found {
            assets.insert(key, fields);
        }
        assets
    }
}

impl Ctx {
    /// The game objects of a file, read once.
    pub fn graph_file(&mut self, file: usize) {
        if self.graphed.insert(file) {
            self.graph.add_file(&mut self.data, file);
        }
    }

    /// GameData.Components: the game's MonoBehaviours on and under a root (inactive ones too),
    /// with their path, fields and the transforms their Transform fields point to.
    pub fn components(&mut self, root: Id) -> Vec<Value> {
        self.graph_file(root.0);
        let mut out = Vec::new();
        for (id, node, namespace, _) in self.graph.behaviours_in_children(&mut self.data, root) {
            if namespace != "SpaceCraft" {
                continue;
            }
            let Some(b) = self.behaviour(id) else { continue };
            let mut c = Map::new();
            c.insert("type".into(), json!(b.class));
            c.insert("path".into(), json!(self.graph.path(root, node)));
            c.insert("fields".into(), Value::Object(self.fields(id.0, &b.fields, 0)));
            let poses = self.poses(root, id.0, &b.fields);
            if !poses.is_empty() {
                c.insert("poses".into(), Value::Object(poses));
            }
            out.push(Value::Object(c));
        }
        out
    }

    /// GameData.Poses: serialized Transform fields, relative to the root.
    fn poses(&mut self, root: Id, file: usize, fields: &[(String, Raw)]) -> Map<String, Value> {
        let (rp, rr, rs) = self.graph.world(root);
        let inverse = crate::geometry::conj(rr);
        let mut poses = Map::new();
        for (name, v) in fields.iter().rev() {
            let Raw::Ref(p, t) = v else { continue };
            if t.namespace != "UnityEngine" || t.name != "Transform" || poses.contains_key(name) {
                continue;
            }
            let Some(target) = self.data.resolve(file, *p) else { continue };
            let Some(node) = self.graph.node_of_transform(target) else { continue };
            let (tp, tr, _) = self.graph.world(node);
            let d = crate::geometry::rotate(inverse, [tp[0] - rp[0], tp[1] - rp[1], tp[2] - rp[2]]);
            let local = [d[0] / rs[0], d[1] / rs[1], d[2] / rs[2]];
            let rotation = crate::geometry::mul(inverse, tr);
            poses.insert(name.clone(), json!({"position": local.map(single).to_vec(), "rotation": rotation.map(single).to_vec()}));
        }
        poses
    }

    /// GameData.GroupPrefabs: each group's prefab, whether it has a Rigidbody, its components.
    pub fn group_prefabs(&mut self) -> Map<String, Value> {
        let mut out = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let Some(root) = prefab else { continue };
            self.graph_file(root.0);
            let Some(node) = self.graph.nodes.get(&root) else { continue };
            let name = node.name.clone();
            let rigidbody = node.components.iter().any(|(_, c)| *c == unity::RIGIDBODY);
            let components = self.components(root);
            out.insert(gid, json!({"prefab": name, "rigidbody": rigidbody, "components": components}));
        }
        out
    }
}

pub fn run(args: &[String]) {
    let (Some(game), Some(out)) = (args.first(), args.get(1)) else {
        eprintln!("usage: pcds-server read-game GAME_DIR OUT_DIR");
        std::process::exit(2);
    };
    let mut ctx = Ctx::open(Path::new(game)).unwrap_or_else(|e| {
        eprintln!("read-game: {e}");
        std::process::exit(1)
    });
    let started = std::time::Instant::now();
    let assets = ctx.assets();
    eprintln!("read-game: {} assets in {:?}", assets.len(), started.elapsed());
    let group_prefabs = ctx.group_prefabs();
    eprintln!("read-game: {} group prefabs in {:?}", group_prefabs.len(), started.elapsed());
    let mut misread: Vec<_> = ctx.misread.iter().collect();
    misread.sort_by(|a, b| b.1.cmp(a.1));
    if !misread.is_empty() {
        eprintln!("read-game: layouts that did not fit their data: {misread:?}");
    }
    std::fs::create_dir_all(out).expect("create OUT_DIR");
    let gamedata = json!({"assets": assets, "groupPrefabs": group_prefabs});
    std::fs::write(Path::new(out).join("gamedata.json"), serde_json::to_string_pretty(&gamedata).unwrap()).expect("write gamedata.json");
}
