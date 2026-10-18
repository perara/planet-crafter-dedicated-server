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
use super::unity::{self, Data, Id};

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
                    // `IsGameObjectType(so) ? Ref(so) : null`: a null string, so JSON null for
                    // other ScriptableObjects.
                    None => Some(if namespace == "SpaceCraft" { json!(format!("{script}:{}", head.name)) } else { Value::Null }),
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
            Raw::Unset => Some(Value::Null),
            Raw::List(items) => {
                if depth > MAX_DEPTH {
                    return None;
                }
                Some(Value::Array(items.iter().map(|i| self.value(file, i, depth + 1).unwrap_or(Value::Null)).collect()))
            }
            // GameData.cs writes a dictionary as [key, value] pairs; Unity's SerializedDictionary
            // stores the keys and the values as two lists.
            Raw::Fields(t, fields) if t.name == "SerializedDictionary`2" => {
                let (Some(Raw::List(keys)), Some(Raw::List(values))) = (super::netdata::field(fields, "m_Keys"), super::netdata::field(fields, "m_Values")) else { return None };
                let pairs = keys.iter().zip(values).map(|(k, v)| json!([self.value(file, k, depth + 1).unwrap_or(Value::Null), self.value(file, v, depth + 1).unwrap_or(Value::Null)])).collect();
                Some(Value::Array(pairs))
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
        for (name, v) in fields {
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
        for (name, v) in fields {
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

/// `pcds read-game GAME_DIR OUT_DIR`.
pub fn run(args: &[String]) {
    let [game, out] = args else {
        eprintln!("usage: pcds read-game GAME_DIR OUT_DIR");
        std::process::exit(2);
    };
    match read_game(Path::new(game), Path::new(out)) {
        Ok(version) => println!("{out}: game tables for {version}"),
        Err(e) => {
            eprintln!("read-game: {e}");
            std::process::exit(1)
        }
    }
}

fn write(out: &Path, name: &str, text: &str) -> Result<(), String> {
    std::fs::write(out.join(name), text).map_err(|e| format!("{}: {e}", out.join(name).display()))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("JSON values serialize")
}

/// Reads every table the server needs from the game's files into OUT (netdata.json,
/// rpcs.json, gamedata.json, connection-request.hex, buildid); returns the game version.
/// Anything it cannot read is an error.
pub fn read_game(game: &Path, out: &Path) -> Result<String, String> {
    let mut ctx = Ctx::open(game).map_err(|e| e.to_string())?;
    let assets = ctx.assets();
    let group_prefabs = ctx.group_prefabs();
    let mut misread: Vec<_> = ctx.misread.iter().collect();
    if !misread.is_empty() {
        misread.sort_by(|a, b| b.1.cmp(a.1));
        return Err(format!("classes whose layout does not fit their data: {misread:?}"));
    }
    let planets = ctx.planets(&assets);
    let groups = ctx.group_table();
    let versions = ctx.message_versions();
    let (config_file, config) = ctx.network_config().ok_or("no NetworkManager with a NetworkConfig")?;
    let tick_rate = super::netdata::field(&config, "TickRate").cloned().and_then(|t| ctx.value(config_file, &t, 1)).ok_or("no NetworkConfig.TickRate")?;
    let player_root = match super::netdata::field(&config, "PlayerPrefab") {
        Some(Raw::Ref(p, _)) => ctx.data.resolve(config_file, *p),
        _ => None,
    }
    .ok_or("no NetworkConfig.PlayerPrefab")?;
    let player_hash = ctx.prefab_hash(player_root).ok_or("the player prefab has no NetworkObject")?;
    let ggm = ctx.data.file("globalgamemanagers").map_err(|e| e.to_string())?;
    let settings = ctx.data.files[ggm].objects.iter().find(|o| o.class_id == 129).map(|o| o.path).ok_or("no PlayerSettings")?;
    let file = &ctx.data.files[ggm];
    let version = super::typetree::bundle_version(&file.unity_version, file.data(file.object(settings).ok_or("no PlayerSettings")?))?;
    let scene = ctx.scene_objects()?;
    let player = ctx.prefab(player_root, true).map_err(|e| format!("player prefab: {e}"))?;
    let handler = ctx.find_behaviour(&["level4"], "SpaceCraft", "WorldUnitsHandler");
    let units = handler.and_then(|h| Some((h, ctx.behaviour(h)?))).and_then(|(h, b)| match super::netdata::field(&b.fields, "_planetUnitsNetworkObject") {
        Some(Raw::Ref(p, _)) => ctx.data.resolve(h.0, *p),
        _ => None,
    });
    let units = ctx.prefab(units.ok_or("no WorldUnitsHandler._planetUnitsNetworkObject")?, false).map_err(|e| format!("planet units prefab: {e}"))?;
    let netdata = json!({"gameVersion": version, "tickRate": tick_rate, "playerPrefab": player_hash, "messageVersions": versions, "sceneObjects": scene, "prefabs": {"Player": player, "WorldUnitsPlanet": units}, "planets": planets, "groups": groups});
    let rpcs = super::rpcs::rpcs(&ctx.layouts.asm);
    // ConnectionRequestMessage as a client sends it (for the scripted test players): the
    // message versions, the config hash, the connection data "version|0".
    let config_hash = ctx.config_hash(config_file, &config).map_err(|e| format!("config hash: {e}"))?;
    let mut request = crate::wire::Writer::new();
    request.packed_i32(versions.len() as i32);
    for v in &versions {
        request.u32(v[0].as_u64().unwrap_or(0) as u32).packed_i32(v[1].as_i64().unwrap_or(0) as i32);
    }
    let data = format!("{version}|0");
    request.u64(config_hash).i32(data.len() as i32).bytes(data.as_bytes());
    let request: String = request.0.iter().map(|b| format!("{b:02x}")).collect();
    // The build these tables are for: the game code's hash, as `pcds` checks it.
    let build = crate::cli::build_id(game)?;
    let layouts = ctx.world_object_layouts(&assets, &group_prefabs).map_err(|e| format!("layouts: {e}"))?;
    let prime = assets.get("PlanetData:PlanetData-Prime").and_then(|p| p["sceneName"].as_str()).ok_or("no PlanetData-Prime scene")?.to_string();
    let singletons = ctx.singletons(&prime, player_root).map_err(|e| format!("singletons: {e}"))?;
    let resources = ctx.find_in_scenes(&["BootScene", "GameMainScene", &prime], "SpaceCraft", "PanelsResources")?.ok_or("no PanelsResources")?;
    let panels = ctx.panels(resources).map_err(|e| format!("panels: {e}"))?;
    let gamedata = json!({"assets": assets, "groupPrefabs": group_prefabs, "layouts": layouts, "singletons": singletons, "panels": panels});
    // scenes/<planet>.json for the planets anyone can play (no DLC app id).
    let mut scenes = Vec::new();
    for planet in planets.iter().filter(|p| p["steamAppId"] == 0) {
        let id = planet["id"].as_str().unwrap_or_default();
        let data = assets.get(&format!("PlanetData:PlanetData-{id}")).ok_or(format!("no PlanetData-{id}"))?;
        scenes.push((id.to_string(), ctx.planet_scenes(&assets, data).map_err(|e| format!("scenes/{id}.json: {e}"))?));
    }
    std::fs::create_dir_all(out.join("scenes")).map_err(|e| format!("{}: {e}", out.display()))?;
    write(out, "netdata.json", &pretty(&netdata))?;
    write(out, "rpcs.json", &pretty(&Value::Object(rpcs)))?;
    write(out, "gamedata.json", &pretty(&gamedata))?;
    for (id, scene) in &scenes {
        write(out, &format!("scenes/{id}.json"), &pretty(scene))?;
    }
    write(out, "connection-request.hex", &request)?;
    write(out, "buildid", &build)?;
    Ok(version)
}

/// Debug: a script class's serialized field layout (pcds-server native-layout GAME_DIR NS.CLASS).
pub fn layout(args: &[String]) {
    let ctx = Ctx::open(Path::new(&args[0])).expect("open game");
    let (ns, class) = args[1].rsplit_once('.').unwrap_or(("", &args[1]));
    let mut layouts = Layouts::new(ctx.layouts.asm.clone());
    let fields = layouts.fields(&TypeName { scope: None, namespace: ns.into(), name: class.into() });
    for f in fields.iter() {
        println!("{} {:?}", f.name, f.kind);
    }
    if let Some((a, i)) = ctx.layouts.asm.resolve(&TypeName { scope: None, namespace: ns.into(), name: class.into() }) {
        for f in &ctx.layouts.asm.0[a].types()[i].fields {
            println!("  declared: {} flags={:#x} {:?} attrs={:?}", f.name, f.flags, f.sig, f.attributes.iter().map(|x| format!("{}.{}", x.ty.namespace, x.ty.name)).collect::<Vec<_>>());
        }
    }
}

/// Debug: a method's IL (pcds-server native-il GAME_DIR NS.CLASS METHOD).
pub fn il(args: &[String]) {
    let ctx = Ctx::open(Path::new(&args[0])).expect("open game");
    let (ns, class) = args[1].rsplit_once('.').unwrap_or(("", &args[1]));
    let t = TypeName { scope: None, namespace: ns.into(), name: class.into() };
    let asm = &ctx.layouts.asm;
    let a = asm.assembly_of(&t).expect("no such type");
    for m in asm.def(&t).unwrap().methods.iter().filter(|m| m.name == args[2]) {
        println!("{}({:?})", m.name, m.params);
        for line in super::netvars::disassemble(a, a.il(m).unwrap_or_default()) {
            println!("  {line}");
        }
    }
}
