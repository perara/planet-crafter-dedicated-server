//! The game tables read from the game's own files, with no game running:
//! `pcds read-game GAME_DIR OUT_DIR` writes every table the server loads (what the Wine-era
//! extractor got from the running game), and `tables-diff` holds two such folders side by side.

use std::path::Path;
use std::rc::Rc;

use serde_json::{json, Map, Value};

use super::dotnet::{Assemblies, TypeName};
use super::graph::Graph;
use super::serial::{enum_name, read_fields, Layouts, Raw};
use super::unity::{self, Data, Id};

/// The format of what read_game writes, in its `tables-format` file: one more whenever any
/// file it writes changes (a field, a section, a value read another way). A server loads only
/// tables of its own format (main.rs), and `pcds update` reads them again with the active
/// bank's program when the format differs. 4: scenes/<planet>.json for the DLC planets too, and
/// the "toxicAreas" section (Toxicity's).
pub const TABLES_FORMAT: u32 = 4;

/// GameData.MaxDepth.
const MAX_DEPTH: usize = 6;

/// A MonoBehaviour (or ScriptableObject) as read: its script, game object, name and fields.
pub struct Behaviour {
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
    /// Group id -> its prefab's root game object (GroupData.associatedGameObject).
    pub groups: Vec<(String, Option<Id>)>,
    /// Files whose game objects are in the graph.
    graphed: std::collections::HashSet<usize>,
}

/// The shortest decimal that reads back as this float (as the game's JSON writes floats).
pub fn single(f: f32) -> Value {
    format!("{f}").parse::<f64>().map(|v| json!(v)).unwrap_or(Value::Null)
}

impl Ctx {
    pub fn open(game: &Path) -> std::io::Result<Ctx> {
        let dir = game.join("Planet Crafter_Data");
        let data = Data::open(&dir)?;
        let asm = Assemblies::load_dir(&dir.join("Managed"))?;
        Ok(Ctx { data, graph: Graph::default(), layouts: Layouts::new(Rc::new(asm)), groups: Vec::new(), graphed: Default::default() })
    }

    /// A MonoBehaviour with its fields; an error unless its script's field layout fits its
    /// data exactly.
    pub fn behaviour(&mut self, id: Id) -> Result<Behaviour, String> {
        let file = self.data.files[id.0].name.clone();
        let (namespace, class, assembly) = self.graph.script(&mut self.data, id)?.ok_or_else(|| format!("{file}: MonoBehaviour {} has no script", id.1))?.clone();
        let fields = self.layouts.fields(&TypeName { scope: Some(assembly.trim_end_matches(".dll").to_string()), namespace: namespace.clone(), name: class.clone() });
        let mut r = self.data.reader(id).ok_or_else(|| format!("{file}: no object {}", id.1))?;
        let head = unity::behaviour(&mut r).ok_or_else(|| format!("{file}: unreadable MonoBehaviour {}", id.1))?;
        let values = read_fields(&fields, &mut r);
        let fits = values.is_some() && r.pos == r.b.len();
        let values = values.filter(|_| fits).ok_or_else(|| format!("{namespace}.{class} ({file} object {}): its field layout does not fit its data", id.1))?;
        let game_object = self.data.resolve(id.0, head.game_object)?;
        Ok(Behaviour { class, game_object, enabled: head.enabled, name: head.name, fields: values })
    }

    fn game_object_name(&mut self, id: Id) -> Result<String, String> {
        self.data.reader(id).and_then(|mut r| unity::game_object(&mut r)).map(|g| g.name).ok_or_else(|| format!("{}: unreadable GameObject {}", self.data.files[id.0].name, id.1))
    }

    /// GameData.Value of a reference: "Type:name" for game assets, "GameObject:name",
    /// "Type@gameObject" for components; None (left out) for other Unity objects.
    fn reference(&mut self, file: usize, p: unity::PPtr) -> Result<Option<Value>, String> {
        let Some(target) = self.data.resolve(file, p)? else { return Ok(Some(Value::Null)) };
        let class = self.data.class_of(target).expect("resolve finds the object");
        let at = format!("{} object {}", self.data.files[target.0].name, target.1);
        Ok(Some(match class {
            unity::GAME_OBJECT => json!(format!("GameObject:{}", self.game_object_name(target)?)),
            unity::MONO_BEHAVIOUR => {
                let head = self.data.reader(target).and_then(|mut r| unity::behaviour(&mut r)).ok_or_else(|| format!("{}: unreadable MonoBehaviour", at))?;
                let (namespace, script, _) = self.graph.script(&mut self.data, target)?.ok_or_else(|| format!("{}: a MonoBehaviour without a script", at))?.clone();
                match self.data.resolve(target.0, head.game_object)? {
                    // `IsGameObjectType(so) ? Ref(so) : null`: a null string, so JSON null for
                    // other ScriptableObjects.
                    None => if namespace == "SpaceCraft" { json!(format!("{script}:{}", head.name)) } else { Value::Null },
                    Some(go) => json!(format!("{script}@{}", self.game_object_name(go)?)),
                }
            }
            c if unity::is_component(c) => {
                // A component: its data starts with its game object; named by its class.
                let go = self.data.reader(target).and_then(|mut r| r.pptr()).ok_or_else(|| format!("{}: unreadable component", at))?;
                let go = self.data.resolve(target.0, go)?.filter(|g| self.data.class_of(*g) == Some(unity::GAME_OBJECT)).ok_or_else(|| format!("{}: a component without a game object", at))?;
                json!(format!("{}@{}", unity::class_name(c).expect("components are named"), self.game_object_name(go)?))
            }
            // `case UnityEngine.Object _: return null`: left out.
            c if unity::class_name(c).is_some() => return Ok(None),
            c => return Err(format!("{}: unknown Unity class id {c} (add it to native/unity.rs class_name, and is_component if it is one)", at)),
        }))
    }

    /// GameData.Value: the value as JSON, None when the field is left out (as the game's GameData.Value leaves it).
    pub fn value(&mut self, file: usize, v: &Raw, depth: usize) -> Result<Option<Value>, String> {
        Ok(Some(match v {
            Raw::Bool(b) => json!(b),
            Raw::Int(i) => json!(i),
            Raw::UInt(u) => json!(u),
            Raw::Float(f) => json!(f),
            Raw::Single(f) => single(*f),
            Raw::Str(s) => json!(s),
            Raw::Enum(v, names, flags) => json!(enum_name(*v, names, *flags)),
            Raw::Ref(p, _) => return self.reference(file, *p),
            Raw::Json(j) => j.clone(),
            Raw::Omit => return Ok(None),
            Raw::Unset => Value::Null,
            Raw::List(items) => {
                if depth > MAX_DEPTH {
                    return Ok(None);
                }
                let mut out = Vec::new();
                for i in items {
                    out.push(self.value(file, i, depth + 1)?.unwrap_or(Value::Null));
                }
                Value::Array(out)
            }
            // GameData.cs writes a dictionary as [key, value] pairs; Unity's SerializedDictionary
            // stores the keys and the values as two lists.
            Raw::Fields(t, fields) if t.name == "SerializedDictionary`2" => {
                let (Some(Raw::List(keys)), Some(Raw::List(values))) = (super::netdata::field(fields, "m_Keys"), super::netdata::field(fields, "m_Values")) else { return Err(format!("a SerializedDictionary without m_Keys and m_Values lists: {fields:?}")) };
                if keys.len() != values.len() {
                    return Err(format!("a SerializedDictionary with {} keys and {} values", keys.len(), values.len()));
                }
                let mut pairs = Vec::new();
                for (k, v) in keys.iter().zip(values) {
                    pairs.push(json!([self.value(file, k, depth + 1)?.unwrap_or(Value::Null), self.value(file, v, depth + 1)?.unwrap_or(Value::Null)]));
                }
                Value::Array(pairs)
            }
            Raw::Fields(t, fields) => {
                if depth > MAX_DEPTH || t.namespace != "SpaceCraft" {
                    return Ok(None);
                }
                Value::Object(self.fields(file, fields, depth)?)
            }
        }))
    }

    /// GameData.Fields: each field's value one level deeper; fields of derived classes win.
    pub fn fields(&mut self, file: usize, fields: &[(String, Raw)], depth: usize) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for (name, v) in fields {
            if out.contains_key(name) {
                continue;
            }
            if let Some(j) = self.value(file, v, depth + 1).map_err(|e| format!("{name}: {e}"))? {
                out.insert(name.clone(), j);
            }
        }
        Ok(out)
    }

    /// GameData.Assets: every game ScriptableObject, keyed "Type:name".
    pub fn assets(&mut self) -> Result<Map<String, Value>, String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.data.dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".assets")).collect();
        names.sort();
        let mut found: Vec<(String, String, Value)> = Vec::new();
        for name in names {
            let f = self.data.file(&name).map_err(|e| format!("{name}: {e}"))?;
            let behaviours: Vec<i64> = self.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
            for path in behaviours {
                let id = Id(f, path);
                // ScriptableObjects only: no game object. The script read first is cheap.
                let head = self.data.reader(id).and_then(|mut r| unity::behaviour(&mut r)).ok_or_else(|| format!("{name}: unreadable MonoBehaviour {path}"))?;
                if !head.game_object.is_null() || self.graph.script(&mut self.data, id)?.is_none_or(|s| s.0 != "SpaceCraft") {
                    continue;
                }
                let b = self.behaviour(id)?;
                if b.class.starts_with("GroupData") {
                    let gid = match super::netdata::field(&b.fields, "id") {
                        Some(Raw::Str(s)) => s.clone(),
                        other => return Err(format!("{}:{}: GroupData.id is {other:?}", b.class, b.name)),
                    };
                    let prefab = match super::netdata::field(&b.fields, "associatedGameObject") {
                        Some(Raw::Ref(p, _)) => self.data.resolve(f, *p)?,
                        other => return Err(format!("{}:{}: GroupData.associatedGameObject is {other:?}", b.class, b.name)),
                    };
                    self.groups.push((gid, prefab));
                }
                let fields = Value::Object(self.fields(f, &b.fields, 0).map_err(|e| format!("{}:{}: {e}", b.class, b.name))?);
                found.push((format!("{}:{}", b.class, b.name), b.name, fields));
            }
        }
        // OrderBy(name), later ones replacing earlier ones under the same key.
        found.sort_by(|a, b| a.1.cmp(&b.1));
        let mut assets = Map::new();
        for (key, _, fields) in found {
            assets.insert(key, fields);
        }
        Ok(assets)
    }
}

impl Ctx {
    /// The game objects of a file, read once.
    pub fn graph_file(&mut self, file: usize) -> Result<(), String> {
        if !self.graphed.contains(&file) {
            self.graph.add_file(&mut self.data, file)?;
            self.graphed.insert(file);
        }
        Ok(())
    }

    /// GameData.Components: the game's MonoBehaviours on and under a root (inactive ones too),
    /// with their path, fields and the transforms their Transform fields point to.
    pub fn components(&mut self, root: Id) -> Result<Vec<Value>, String> {
        self.graph_file(root.0)?;
        let mut out = Vec::new();
        for (id, node, namespace, _) in self.graph.behaviours_in_children(&mut self.data, root)? {
            if namespace != "SpaceCraft" {
                continue;
            }
            let b = self.behaviour(id)?;
            let mut c = Map::new();
            c.insert("type".into(), json!(b.class));
            c.insert("path".into(), json!(self.graph.path(root, node)));
            c.insert("fields".into(), Value::Object(self.fields(id.0, &b.fields, 0).map_err(|e| format!("{}: {e}", b.class))?));
            let poses = self.poses(root, id.0, &b.fields)?;
            if !poses.is_empty() {
                c.insert("poses".into(), Value::Object(poses));
            }
            // Below the root: where the component's own game object is, relative to the root.
            if node != root {
                c.insert("pose".into(), self.local_pose(root, node)?);
            }
            out.push(Value::Object(c));
        }
        Ok(out)
    }

    /// GameData.Poses: serialized Transform fields (and GameObject fields, by their transform),
    /// relative to the root.
    fn poses(&mut self, root: Id, file: usize, fields: &[(String, Raw)]) -> Result<Map<String, Value>, String> {
        let mut poses = Map::new();
        for (name, v) in fields {
            let Raw::Ref(p, t) = v else { continue };
            if t.namespace != "UnityEngine" || !matches!(t.name.as_str(), "Transform" | "GameObject") || poses.contains_key(name) {
                continue;
            }
            let Some(target) = self.data.resolve(file, *p)? else { continue };
            let node = if t.name == "GameObject" {
                // Only game objects of the prefab have a place relative to its root.
                if !self.graph.nodes.contains_key(&target) {
                    continue;
                }
                target
            } else {
                self.graph.node_of_transform(target).ok_or_else(|| format!("{name}: {target:?} is not a transform of a file read"))?
            };
            poses.insert(name.clone(), self.local_pose(root, node)?);
        }
        Ok(poses)
    }

    /// A game object's position and rotation in its root's space.
    fn local_pose(&self, root: Id, node: Id) -> Result<Value, String> {
        let (rp, rr, rs) = self.graph.world(root)?;
        let inverse = crate::geometry::conj(rr);
        let (tp, tr, _) = self.graph.world(node)?;
        let d = crate::geometry::rotate(inverse, [tp[0] - rp[0], tp[1] - rp[1], tp[2] - rp[2]]);
        let local = [d[0] / rs[0], d[1] / rs[1], d[2] / rs[2]];
        let rotation = crate::geometry::mul(inverse, tr);
        Ok(json!({"position": local.map(single).to_vec(), "rotation": rotation.map(single).to_vec()}))
    }

    /// GameData.GroupPrefabs: each group's prefab, whether it has a Rigidbody, its components.
    pub fn group_prefabs(&mut self) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let Some(root) = prefab else { continue };
            self.graph_file(root.0)?;
            let node = self.graph.nodes.get(&root).ok_or_else(|| format!("{gid}: its associatedGameObject is not a GameObject"))?;
            let name = node.name.clone();
            let rigidbody = node.components.iter().any(|(_, c)| *c == unity::RIGIDBODY);
            let components = self.components(root).map_err(|e| format!("{gid}: {e}"))?;
            out.insert(gid, json!({"prefab": name, "rigidbody": rigidbody, "components": components}));
        }
        Ok(out)
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
/// rpcs.json, gamedata.json, scenes/ (every planet's, DLC planets included), wrecks.json, connection-request.hex, buildid); returns
/// the game version.
/// Anything it cannot read is an error.
pub fn read_game(game: &Path, out: &Path) -> Result<String, String> {
    let mut ctx = Ctx::open(game).map_err(|e| e.to_string())?;
    let assets = ctx.assets()?;
    let group_prefabs = ctx.group_prefabs()?;
    let planets = ctx.planets(&assets)?;
    let groups = ctx.group_table()?;
    let versions = ctx.message_versions()?;
    let (config_file, config) = ctx.network_config()?;
    let tick_rate = super::netdata::field(&config, "TickRate").cloned().ok_or("no NetworkConfig.TickRate")?;
    let tick_rate = ctx.value(config_file, &tick_rate, 1)?.ok_or("NetworkConfig.TickRate left out")?;
    let player_root = match super::netdata::field(&config, "PlayerPrefab") {
        Some(Raw::Ref(p, _)) => ctx.data.resolve(config_file, *p)?,
        _ => None,
    }
    .ok_or("no NetworkConfig.PlayerPrefab")?;
    let player_hash = ctx.prefab_hash(player_root)?.ok_or("the player prefab has no NetworkObject")?;
    let ggm = ctx.data.file("globalgamemanagers").map_err(|e| e.to_string())?;
    let settings = ctx.data.files[ggm].objects.iter().find(|o| o.class_id == 129).map(|o| o.path).ok_or("no PlayerSettings")?;
    let file = &ctx.data.files[ggm];
    let version = super::typetree::bundle_version(&file.unity_version, file.data(file.object(settings).ok_or("no PlayerSettings")?))?;
    let scene = ctx.scene_objects()?;
    let player = ctx.prefab(player_root, true).map_err(|e| format!("player prefab: {e}"))?;
    let handler = ctx.find_behaviour(&["level4"], "SpaceCraft", "WorldUnitsHandler")?.ok_or("no WorldUnitsHandler in level4")?;
    let units = match super::netdata::field(&ctx.behaviour(handler)?.fields, "_planetUnitsNetworkObject") {
        Some(Raw::Ref(p, _)) => ctx.data.resolve(handler.0, *p)?,
        other => return Err(format!("WorldUnitsHandler._planetUnitsNetworkObject: {other:?}")),
    };
    let units = ctx.prefab(units.ok_or("no WorldUnitsHandler._planetUnitsNetworkObject")?, false).map_err(|e| format!("planet units prefab: {e}"))?;
    let netdata = json!({"gameVersion": version, "tickRate": tick_rate, "playerPrefab": player_hash, "messageVersions": versions, "sceneObjects": scene, "prefabs": {"Player": player, "WorldUnitsPlanet": units}, "planets": planets, "groups": groups});
    let rpcs = super::rpcs::rpcs(&ctx.layouts.asm).map_err(|e| format!("rpcs.json: {e}"))?;
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
    let build = crate::tables::build_id(game)?;
    let layouts = ctx.world_object_layouts(&assets, &group_prefabs).map_err(|e| format!("layouts: {e}"))?;
    let prime = assets.get("PlanetData:PlanetData-Prime").and_then(|p| p["sceneName"].as_str()).ok_or("no PlanetData-Prime scene")?.to_string();
    let singletons = ctx.singletons(&prime, player_root).map_err(|e| format!("singletons: {e}"))?;
    let resources = ctx.find_in_scenes(&["BootScene", "GameMainScene", &prime], "SpaceCraft", "PanelsResources")?.ok_or("no PanelsResources")?;
    let panels = ctx.panels(resources).map_err(|e| format!("panels: {e}"))?;
    let gamedata = json!({"assets": assets, "groupPrefabs": group_prefabs, "layouts": layouts, "singletons": singletons, "panels": panels});
    // scenes/<planet>.json for every planet of the PlanetList. The DLC planets' scenes are in the same
    // base files and have the same formats, so the same reader serves them; the file stays in these
    // tables, which are made on the owner's machine and never published.
    let mut scenes = Vec::new();
    for planet in planets.iter() {
        let id = planet["id"].as_str().unwrap_or_default();
        let data = assets.get(&format!("PlanetData:PlanetData-{id}")).ok_or(format!("no PlanetData-{id}"))?;
        scenes.push((id.to_string(), ctx.planet_scenes(&assets, data).map_err(|e| format!("scenes/{id}.json: {e}"))?));
    }
    // wrecks.json: what the server needs to lay a wreck out from its own seed (Tessera's tiles).
    let wrecks = ctx.wrecks().map_err(|e| format!("wrecks: {e}"))?;
    std::fs::create_dir_all(out.join("scenes")).map_err(|e| format!("{}: {e}", out.display()))?;
    write(out, "netdata.json", &pretty(&netdata))?;
    write(out, "rpcs.json", &pretty(&Value::Object(rpcs)))?;
    write(out, "gamedata.json", &pretty(&gamedata))?;
    for (id, scene) in &scenes {
        write(out, &format!("scenes/{id}.json"), &pretty(scene))?;
    }
    write(out, "wrecks.json", &pretty(&wrecks))?;
    write(out, "connection-request.hex", &request)?;
    write(out, "buildid", &build)?;
    write(out, "tables-format", &TABLES_FORMAT.to_string())?;
    Ok(version)
}

/// Debug: a script class's serialized field layout (pcds native-layout GAME_DIR NS.CLASS).
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

/// Debug: a method's IL, every method's for `*` (pcds native-il GAME_DIR [ASSEMBLY!]NS.CLASS METHOD).
pub fn il(args: &[String]) {
    let ctx = Ctx::open(Path::new(&args[0])).expect("open game");
    // ASSEMBLY!NS.CLASS: a class of that assembly (names several assemblies define).
    let (scope, name) = match args[1].split_once('!') {
        Some((a, n)) => (Some(a.to_string()), n),
        None => (None, args[1].as_str()),
    };
    let (ns, class) = name.rsplit_once('.').unwrap_or(("", name));
    let t = TypeName { scope, namespace: ns.into(), name: class.into() };
    let asm = &ctx.layouts.asm;
    let a = asm.assembly_of(&t).expect("no such type");
    let def = asm.def(&t).unwrap();
    if args[2] == "*" {
        println!("class {} : {:?} {:?}", t.full(), def.extends, def.interfaces);
        for f in &def.fields {
            // An array initializer's data: as many bytes as its type's ClassLayout size.
            let size = match &f.sig {
                super::dotnet::TypeSig::ValueType(n) => asm.def(&TypeName { scope: t.scope.clone(), ..n.clone() }).and_then(|d| d.class_layout).map(|l| l.1 as usize),
                super::dotnet::TypeSig::I8 | super::dotnet::TypeSig::U8 if f.flags & 0x100 != 0 => Some(8),
                super::dotnet::TypeSig::I4 | super::dotnet::TypeSig::U4 if f.flags & 0x100 != 0 => Some(4),
                _ => None,
            };
            let data = size.and_then(|n| a.field_data(&t.namespace, &t.name, &f.name, n)).map(|b| format!(" data {b:02x?}")).unwrap_or_default();
            println!("  field {} {:?} flags={:#x}{}{data}", f.name, f.sig, f.flags, f.constant.map(|c| format!(" = {c}")).unwrap_or_default());
        }
    }
    for m in def.methods.iter().filter(|m| m.name == args[2] || args[2] == "*") {
        println!("{}({:?}) flags={:#x} {} IL bytes", m.name, m.params, m.flags, a.il(m).map_or(0, |b| b.len()));
        println!("  locals {:?}", a.locals(m));
        for line in super::netvars::disassemble(a, a.il(m).unwrap_or_default()) {
            println!("  {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    /// The whole extraction from the installed game writes the tables the other tests run on,
    /// without a difference. It reads every game file (about 40 s), so it runs by hand:
    /// cargo test --release read_game_writes -- --ignored
    #[test]
    #[ignore]
    fn read_game_writes_the_test_tables() {
        let (Some(game), Some(expected)) = (crate::testkit::game_dir(), crate::test_data()) else { return };
        let out = std::env::temp_dir().join(format!("pcds-read-game-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let version = super::read_game(&game, &out).unwrap();
        let differences = crate::native::diff::diff(expected, out.to_str().unwrap(), None, 8).unwrap();
        std::fs::remove_dir_all(&out).unwrap();
        assert_eq!(differences, 0, "the tables read from game {version} differ from {expected}");
    }
}
