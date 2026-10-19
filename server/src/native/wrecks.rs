//! wrecks.json from the game files, with no game running: the generators' prefabs, the
//! RandomizePropData lists their walks take, and each seed's layout from the Tessera port (tessera.rs), as the game lays it out.
//! `native-reach` (reach below) measures how much of the game's IL a port like that has to cover.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use super::netdata::field;
use super::serial::Raw;
use super::tables::{single as num, Ctx};
use super::tessera;
use super::unity::{self, Id};
use crate::geometry::{from_euler_y, matrix_mul, trs, MathRandom, Matrix};

impl Ctx {
    /// ProceduralInstancesHandler._generators: each entry's generator prefab root.
    pub fn wreck_generators(&mut self) -> Result<Vec<Id>, String> {
        let scenes = self.data.build_scenes()?;
        let mut handler = None;
        for i in 0..scenes.len() {
            if let Some(h) = self.find_behaviour(&[&format!("level{i}")], "SpaceCraft", "ProceduralInstancesHandler")? {
                handler = Some(h);
                break;
            }
        }
        let handler = handler.ok_or("no ProceduralInstancesHandler in the build's scenes")?;
        let b = self.behaviour(handler).map_err(|e| format!("ProceduralInstancesHandler: unreadable: {e}"))?;
        let Some(Raw::List(entries)) = field(&b.fields, "_generators") else { return Err("ProceduralInstancesHandler._generators: missing".into()) };
        let mut out = Vec::new();
        for e in entries {
            let Raw::Fields(_, f) = e else { return Err("_generators: not a struct".into()) };
            let Some(Raw::Ref(p, _)) = field(f, "generator") else { return Err("_generators: no generator".into()) };
            out.push(self.data.resolve(handler.0, *p)?.ok_or("_generators: null generator")?);
        }
        Ok(out)
    }
}

/// A field's value by name, or what is missing.
fn get<'a>(f: &'a [(String, Raw)], name: &str) -> Result<&'a Raw, String> {
    field(f, name).ok_or_else(|| format!("no field {name}"))
}

fn int(f: &[(String, Raw)], name: &str) -> Result<i64, String> {
    match get(f, name)? {
        Raw::Int(v) | Raw::Enum(v, ..) => Ok(*v),
        Raw::UInt(v) => Ok(*v as i64),
        v => Err(format!("{name}: not an integer: {v:?}")),
    }
}

fn boolean(f: &[(String, Raw)], name: &str) -> Result<bool, String> {
    match get(f, name)? {
        Raw::Bool(b) => Ok(*b),
        v => Err(format!("{name}: not a bool: {v:?}")),
    }
}

fn single(f: &[(String, Raw)], name: &str) -> Result<f32, String> {
    match get(f, name)? {
        Raw::Single(v) => Ok(*v),
        v => Err(format!("{name}: not a float: {v:?}")),
    }
}

/// A Vector3 (floats) or Vector3Int field.
fn vector<T: serde::de::DeserializeOwned>(f: &[(String, Raw)], name: &str) -> Result<[T; 3], String> {
    match get(f, name)? {
        Raw::Json(j) => serde_json::from_value(j.clone()).map_err(|e| format!("{name}: {e}")),
        v => Err(format!("{name}: not a vector: {v:?}")),
    }
}

fn list<'a>(f: &'a [(String, Raw)], name: &str) -> Result<&'a [Raw], String> {
    match get(f, name)? {
        Raw::List(items) => Ok(items),
        v => Err(format!("{name}: not a list: {v:?}")),
    }
}

fn fields(v: &Raw) -> Result<&[(String, Raw)], String> {
    match v {
        Raw::Fields(_, f) => Ok(f),
        v => Err(format!("not a struct: {v:?}")),
    }
}

impl Ctx {
    /// Where a reference field of an object in `file` points (None for null).
    fn target(&mut self, file: usize, v: &Raw) -> Result<Option<Id>, String> {
        match v {
            Raw::Ref(p, _) => self.data.resolve(file, *p),
            v => Err(format!("not a reference: {v:?}")),
        }
    }

    /// The MonoBehaviours of a script class on one game object.
    pub fn components_of(&mut self, node: Id, class: &str) -> Result<Vec<Id>, String> {
        self.graph_file(node.0)?;
        let components = self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for (c, k) in components {
            if k == super::unity::MONO_BEHAVIOUR && self.graph.script(&mut self.data, c)?.is_some_and(|s| s.1 == class) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// A TesseraTile as Tessera uses it: TesseraTileBase.OnAfterDeserialize turns its
    /// faceDetails and offsets into sylvesFaceDetails and sylvesOffsets (for cubes: the same
    /// values, every face a Square).
    pub fn tessera_tile(&mut self, component: Id) -> Result<tessera::TileDef, String> {
        let b = self.behaviour(component).map_err(|e| format!("TesseraTile: unreadable: {e}"))?;
        if b.class != "TesseraTile" {
            return Err(format!("{}: a {} (only cube tiles are ported)", b.name, b.class));
        }
        let f = &b.fields;
        let go = b.game_object.ok_or("TesseraTile without a game object")?;
        self.graph_file(go.0)?;
        let name = self.graph.nodes.get(&go).map(|n| n.name.clone()).unwrap_or_default();
        let mut faces = Vec::new();
        for face in list(f, "faceDetails")? {
            let face = fields(face)?;
            let d = fields(get(face, "faceDetails")?)?;
            let mut paint = [0; 9];
            for (i, n) in ["topLeft", "top", "topRight", "left", "center", "right", "bottomLeft", "bottom", "bottomRight"].iter().enumerate() {
                paint[i] = int(d, n)? as i32;
            }
            for n in ["hexRightAndTopRight", "hexTopLeftAndLeft", "hexLeftAndBottomLeft", "hexBottomRightAndRight"] {
                if int(d, n)? != 0 {
                    return Err(format!("{name}: FaceDetails.{n} set (only square faces are ported)"));
                }
            }
            faces.push(tessera::Face { offset: vector(face, "offset")?, dir: int(face, "faceDir")? as u8, details: tessera::FaceDetails { paint } });
        }
        let offsets = list(f, "offsets")?
            .iter()
            .map(|o| match o {
                Raw::Json(j) => serde_json::from_value(j.clone()).map_err(|e| e.to_string()),
                v => Err(format!("offsets: {v:?}")),
            })
            .collect::<Result<Vec<[i32; 3]>, String>>()?;
        let palette = self.target(component.0, get(f, "palette")?)?;
        Ok(tessera::TileDef {
            id: component,
            name,
            palette,
            faces,
            offsets,
            center: vector(f, "center")?,
            cell_size: vector(f, "cellSize")?,
            rotatable: boolean(f, "rotatable")?,
            reflectable: boolean(f, "reflectable")?,
            rotation_group: int(f, "rotationGroupType")? as i32,
            symmetric: boolean(f, "symmetric")?,
        })
    }

    /// A wreck's TesseraGenerator: its settings, tiles and constraints. Settings the port
    /// does not cover fail.
    pub fn tessera_generator(&mut self, component: Id) -> Result<tessera::Generator, String> {
        let b = self.behaviour(component).map_err(|e| format!("TesseraGenerator: unreadable: {e}"))?;
        let f = b.fields.clone();
        let file = component.0;
        let go = b.game_object.ok_or("TesseraGenerator without a game object")?;
        let expect = |what: &str, v: i64, want: i64| if v == want { Ok(()) } else { Err(format!("TesseraGenerator.{what} is {v}: only {want} is ported")) };
        expect("modelType", int(&f, "modelType")?, 0)?;
        expect("algorithm", int(&f, "algorithm")?, 0)?;
        expect("failureMode", int(&f, "failureMode")?, 0)?;
        expect("backtrack", boolean(&f, "backtrack")? as i64, 0)?;
        expect("stepLimit", int(&f, "stepLimit")?, 0)?;
        if self.target(file, get(&f, "surfaceMesh")?)?.is_some() {
            return Err("TesseraGenerator.surfaceMesh: only cube grids are ported".into());
        }
        let mut tiles = Vec::new();
        for e in list(&f, "tiles")? {
            let e = fields(e)?;
            let Some(tile) = self.target(file, get(e, "tile")?)? else { continue };
            tiles.push((self.tessera_tile(tile)?, single(e, "weight")?));
        }
        let sky_box = match self.target(file, get(&f, "skyBox")?)? {
            Some(t) => Some(self.tessera_tile(t)?),
            None => None,
        };
        let mut constraints = Vec::new();
        self.graph_file(go.0)?;
        for (c, class_id) in self.graph.nodes.get(&go).map(|n| n.components.clone()).unwrap_or_default() {
            if class_id != super::unity::MONO_BEHAVIOUR {
                continue;
            }
            let Some((ns, class, _)) = self.graph.script(&mut self.data, c)?.cloned() else { continue };
            if ns != "Tessera" || class == "TesseraGenerator" {
                continue;
            }
            let cb = self.behaviour(c).map_err(|e| format!("constraint: unreadable: {e}"))?;
            if !cb.enabled {
                continue;
            }
            let cf = cb.fields;
            constraints.push(match class.as_str() {
                "CountConstraint" => {
                    let mut tiles = Vec::new();
                    for t in list(&cf, "tiles")? {
                        tiles.extend(self.target(c.0, t)?);
                    }
                    tessera::ConstraintDef::Count { tiles, comparison: int(&cf, "comparison")? as i32, count: int(&cf, "count")? as i32, eager: boolean(&cf, "eager")? }
                }
                "PathConstraint" => {
                    if boolean(&cf, "hasPathTiles")? || boolean(&cf, "prioritize")? || boolean(&cf, "loops")? || boolean(&cf, "acyclic")? || !boolean(&cf, "connected")? || !boolean(&cf, "hasPathColors")? {
                        return Err(format!("PathConstraint {cf:?}: only connected paths of colours are ported"));
                    }
                    let colors = list(&cf, "pathColors")?
                        .iter()
                        .map(|c| match c {
                            Raw::Int(v) => Ok(*v as i32),
                            v => Err(format!("pathColors: {v:?}")),
                        })
                        .collect::<Result<Vec<i32>, String>>()?;
                    tessera::ConstraintDef::Path { colors, parity: boolean(&cf, "parity")? }
                }
                other => return Err(format!("Tessera constraint {other}: not ported")),
            });
        }
        // TesseraGenerator.palette: the first tile's (TesseraPalette.OnAfterDeserialize makes
        // matchOverrides from the list, ToDictionary failing on a repeated pair).
        let mut palette = tessera::Palette::default();
        if let Some(p) = tiles.first().and_then(|t| t.0.palette) {
            let pb = self.behaviour(p).map_err(|e| format!("TesseraPalette: unreadable: {e}"))?;
            for o in list(&pb.fields, "matchOverridesList")? {
                let o = fields(o)?;
                let key = (int(o, "color1")? as i32, int(o, "color2")? as i32);
                if palette.overrides.insert(key, boolean(o, "isMatch")?).is_some() {
                    return Err(format!("TesseraPalette: match override {key:?} twice"));
                }
            }
        }
        Ok(tessera::Generator {
            palette,
            size: vector(&f, "m_size")?,
            center: vector(&f, "m_center")?,
            cell_size: vector(&f, "m_cellSize")?,
            retries: int(&f, "retries")? as i32,
            search_initial_constraints: boolean(&f, "searchInitialConstraints")?,
            tiles,
            sky_box,
            constraints,
        })
    }
}

/// Matrix4x4 rows 0-2 (Generation.Affine).
fn affine(m: &Matrix) -> Value {
    Value::Array(m[..3].iter().flat_map(|r| r.iter().map(|v| num(*v))).collect())
}

fn v3(v: [f32; 3]) -> Value {
    Value::Array(v.iter().map(|x| num(*x)).collect())
}

/// Matrix4x4.lossyScale for the matrices these are (rotation and scale): column lengths.
fn lossy(m: &Matrix) -> [f32; 3] {
    std::array::from_fn(|j| (m[0][j] * m[0][j] + m[1][j] * m[1][j] + m[2][j] * m[2][j]).sqrt())
}

/// A RandomizePropData as the walk takes it.
struct Slot {
    component: Id,
    /// Its matrix with the walked root at the origin, unrotated, its scale kept.
    m: Matrix,
    network: bool,
    no_count_limited: bool,
    /// RandomData: chance, randomizeRotationY, prop.
    props: Vec<(i32, bool, Option<Id>)>,
    ignore: Vec<Id>,
}

/// Generation.Props: the RandomizePropData lists by the object walked from, keyed as
/// read-game keys them (first come first named, "#n" on a repeated name).
#[derive(Default)]
struct Props {
    keys: HashMap<Id, Option<String>>,
    lists: IndexMap<String, Vec<Slot>>,
    ids: HashMap<Id, (String, usize)>,
}

impl Ctx {
    /// The game object's transform: local position, rotation, scale.
    fn local(&mut self, node: Id) -> Result<([f32; 3], [f32; 4], [f32; 3]), String> {
        self.graph_file(node.0)?;
        let t = self.graph.nodes.get(&node).and_then(|n| self.graph.xforms.get(&n.transform)).ok_or(format!("{node:?}: no transform"))?;
        Ok((t.position, t.rotation, t.scale))
    }

    /// Transform.localToWorldMatrix within its file's hierarchy.
    fn ltw(&mut self, node: Id) -> Result<Matrix, String> {
        let (p, q, s) = self.local(node)?;
        let m = trs(p, q, s);
        Ok(match self.graph.parent(node) {
            Some(parent) => matrix_mul(&self.ltw(parent)?, &m),
            None => m,
        })
    }

    fn node_name(&mut self, node: Id) -> Result<String, String> {
        self.graph_file(node.0)?;
        self.graph.nodes.get(&node).map(|n| n.name.clone()).ok_or(format!("{node:?}: no game object"))
    }

    /// The first MonoBehaviour of a class on a game object (GetComponent).
    fn component(&mut self, node: Id, class: &str) -> Result<Option<Id>, String> {
        Ok(self.components_of(node, class)?.into_iter().next())
    }

    /// GenerateOnTrigger.GetComponentsInChildrenOrdered: the RandomizePropData of active
    /// objects, depth first.
    fn ordered(&mut self, node: Id, out: &mut Vec<Id>) -> Result<(), String> {
        self.graph_file(node.0)?;
        if !self.graph.nodes.get(&node).is_some_and(|n| n.active) {
            return Ok(());
        }
        if let Some(c) = self.component(node, "RandomizePropData")? {
            out.push(c);
        }
        for child in self.graph.children(node) {
            self.ordered(child, out)?;
        }
        Ok(())
    }

    /// GroupsHandler.GetGroupsViaGameObject(prefab)?.GetId(): the last group with that prefab.
    fn group_of(&self, prefab: Id) -> Option<String> {
        self.groups.iter().rev().find(|(_, p)| *p == Some(prefab)).map(|(g, _)| g.clone())
    }

    /// Props.Key.
    fn props_key(&mut self, props: &mut Props, root: Id, name: Option<String>) -> Result<Option<String>, String> {
        if let Some(k) = props.keys.get(&root) {
            return Ok(k.clone());
        }
        let mut found = Vec::new();
        self.ordered(root, &mut found)?;
        let mut key = None;
        if !found.is_empty() {
            let base = match name {
                Some(n) => n,
                None => self.node_name(root)?,
            };
            let mut k = base.clone();
            let mut n = 2;
            while props.lists.contains_key(&k) {
                k = format!("{base}#{n}");
                n += 1;
            }
            props.lists.insert(k.clone(), Vec::new());
            key = Some(k);
        }
        props.keys.insert(root, key.clone());
        let (_, _, scale) = self.local(root)?;
        let origin = matrix_mul(&trs([0.0; 3], [0.0, 0.0, 0.0, 1.0], scale), &tessera::inverse(&self.ltw(root)?));
        for (i, c) in found.iter().enumerate() {
            props.ids.insert(*c, (key.clone().unwrap_or_default(), i));
        }
        for c in found {
            let b = self.behaviour(c).map_err(|e| format!("RandomizePropData: unreadable: {e}"))?;
            let node = b.game_object.ok_or("RandomizePropData without a game object")?;
            let network = boolean(&b.fields, "networkObjects")?;
            let mut randoms = Vec::new();
            for d in list(&b.fields, "props")? {
                let d = fields(d)?;
                randoms.push((int(d, "chancePourcentage")? as i32, boolean(d, "randomizeRotationY")?, self.target(c.0, get(d, "prop")?)?));
            }
            let mut ignore = Vec::new();
            for r in list(&b.fields, "ignoreIfGenerated")? {
                ignore.extend(self.target(c.0, r)?);
            }
            let slot = Slot { component: c, m: matrix_mul(&origin, &self.ltw(node)?), network, no_count_limited: boolean(&b.fields, "noCountLimited")?, props: randoms.clone(), ignore };
            props.lists.get_mut(key.as_ref().expect("a key for a list")).expect("the list").push(slot);
            // The props' own lists, named after them (after this list is complete).
            if !network {
                for (_, _, prop) in randoms {
                    if let Some(prop) = prop {
                        self.props_key(props, prop, None)?;
                    }
                }
            }
        }
        Ok(key)
    }

    /// Props.Finish and the lists as JSON.
    fn props_json(&mut self, props: &Props) -> Value {
        let mut out = Map::new();
        for (key, slots) in &props.lists {
            let mut list = Vec::new();
            for s in slots {
                let randoms: Vec<Value> = s.props.iter().map(|(chance, rotate, prop)| json!([chance, rotate, prop.and_then(|p| self.group_of(p)), if s.network { None } else { prop.and_then(|p| props.keys.get(&p).cloned().flatten()) }])).collect();
                let mut e = json!({"m": affine(&s.m), "network": s.network, "noCountLimited": s.no_count_limited, "props": randoms});
                let ignore: Vec<Value> = s.ignore.iter().filter_map(|c| props.ids.get(c)).map(|(k, i)| json!([k, i])).collect();
                if !ignore.is_empty() {
                    e["ignore"] = Value::Array(ignore);
                }
                list.push(e);
            }
            out.insert(key.clone(), Value::Array(list));
        }
        Value::Object(out)
    }

    /// Generation.Shape of the root's trigger colliders (a prefab root: at its own pose). A
    /// prefab asset is in no scene, so none of its objects is activeInHierarchy: "active" is
    /// false whatever the collider's enabled flag.
    fn triggers(&mut self, root: Id) -> Result<Vec<Value>, String> {
        let (p, q, s) = self.local(root)?;
        let mut out = Vec::new();
        for (c, class_id) in self.graph.nodes.get(&root).map(|n| n.components.clone()).unwrap_or_default() {
            if ![unity::BOX_COLLIDER, unity::SPHERE_COLLIDER, unity::CAPSULE_COLLIDER, unity::MESH_COLLIDER].contains(&class_id) {
                continue;
            }
            let collider = self.data.reader(c).and_then(|mut r| unity::read_collider(class_id, &mut r)).ok_or("unreadable collider")?;
            if !collider.trigger {
                continue;
            }
            let mut shape = json!({"type": unity::class_name(class_id), "active": false, "position": v3(p), "rotation": Value::Array(q.iter().map(|x| num(*x)).collect()), "scale": v3(s)});
            match collider.shape {
                unity::Shape::Box { center, size } => {
                    shape["center"] = v3(center);
                    shape["size"] = v3(size);
                }
                unity::Shape::Sphere { center, radius } => {
                    shape["center"] = v3(center);
                    shape["radius"] = num(radius);
                }
                unity::Shape::Capsule { center, radius, height, direction } => {
                    shape["center"] = v3(center);
                    shape["radius"] = num(radius);
                    shape["height"] = num(height);
                    shape["direction"] = json!(direction);
                }
                unity::Shape::Mesh => {}
            }
            out.push(shape);
        }
        Ok(out)
    }

    /// The scene tiles (TesseraTileBase) of active objects at and under a node, with the
    /// node's matrix given: their TesseraTile and matrix. Pins and volumes are not ported.
    fn scene_tiles(&mut self, node: Id, ltw: &Matrix, out: &mut Vec<(Id, Matrix)>) -> Result<(), String> {
        self.graph_file(node.0)?;
        if !self.graph.nodes.get(&node).is_some_and(|n| n.active) {
            return Ok(());
        }
        for (c, class_id) in self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default() {
            if class_id != unity::MONO_BEHAVIOUR {
                continue;
            }
            let Some((ns, class, _)) = self.graph.script(&mut self.data, c)?.cloned() else { continue };
            if ns != "Tessera" {
                continue;
            }
            match class.as_str() {
                "TesseraTile" => out.push((c, *ltw)),
                "TesseraPinned" | "TesseraVolume" | "TesseraTileBase" | "TesseraHexTile" | "TesseraTrianglePrismTile" | "TesseraSquareTile" => return Err(format!("{} {class} in a wreck: not ported", self.node_name(node)?)),
                _ => {}
            }
        }
        for child in self.graph.children(node) {
            let (p, q, s) = self.local(child)?;
            let m = matrix_mul(ltw, &trs(p, q, s));
            self.scene_tiles(child, &m, out)?;
        }
        Ok(())
    }

    /// GenerateOnTrigger.InstantiateRandomPropsInHierarchy as read-game runs it: the props
    /// it instantiates under the container (`parent`), as (prefab, world matrix). `ignored`:
    /// the RandomizePropData.ignore flags set.
    #[allow(clippy::too_many_arguments)]
    fn walk_props(&self, props: &Props, key: &str, ltw: &Matrix, parent: &Matrix, random: &mut MathRandom, ignored: &mut HashSet<Id>, made: &mut Vec<(Id, Matrix)>) -> Result<(), String> {
        let mut picked = Vec::new();
        for slot in props.lists.get(key).into_iter().flatten() {
            if slot.network {
                ignored.remove(&slot.component);
                continue;
            }
            // RandomizePropData.RetrieveRandomProp.
            if ignored.remove(&slot.component) {
                continue;
            }
            let num = random.next_int(0, 101);
            let mut sum = 0;
            let mut chosen = None;
            for d in &slot.props {
                if num >= sum && num <= sum + d.0 {
                    ignored.extend(slot.ignore.iter().copied());
                    chosen = Some(d);
                    break;
                }
                sum += d.0;
            }
            let Some(&(_, rotate, prop)) = chosen else { continue };
            let mut m = slot.m;
            if rotate {
                m = matrix_mul(&m, &trs([0.0; 3], from_euler_y(random.next_float() * 360.0), [1.0; 3]));
            }
            let prop = prop.ok_or("RandomizePropData picked a null prop")?;
            made.push((prop, matrix_mul(&matrix_mul(parent, ltw), &m)));
            picked.push((prop, m));
        }
        for (prop, m) in picked {
            if let Some(Some(k)) = props.keys.get(&prop) {
                self.walk_props(props, k, &matrix_mul(ltw, &m), parent, random, ignored, made)?;
            }
        }
        Ok(())
    }

    /// Generation.Wrecks: wrecks.json.
    pub fn wrecks(&mut self) -> Result<Value, String> {
        const SEEDS: u32 = 64;
        if self.groups.is_empty() {
            return Err("wrecks: the groups are not read yet".into());
        }
        let mut props = Props::default();
        let mut generators = Vec::new();
        // RandomizePropData.ignore on prefabs stays set from one generation to the next.
        let mut ignored: HashSet<Id> = HashSet::new();
        for root in self.wreck_generators()? {
            let prefab = self.node_name(root)?;
            let wreck = self.component(root, "GenerateOnTrigger")?.ok_or(format!("{prefab}: no GenerateOnTrigger"))?;
            let b = self.behaviour(wreck).map_err(|e| format!("GenerateOnTrigger: unreadable: {e}"))?;
            let tessera = self.target(wreck.0, get(&b.fields, "_generator")?)?.ok_or(format!("{prefab}: no generator"))?;
            let tessera_node = self.behaviour(tessera)?.game_object.ok_or("TesseraGenerator without a game object")?;
            let container = self.target(wreck.0, get(&b.fields, "_randomGeneratedPropsContainer")?)?.ok_or(format!("{prefab}: no props container"))?;
            let root_inverse = tessera::inverse(&self.ltw(root)?);
            let mut children = Vec::new();
            let mut walked = Vec::new();
            for child in self.graph.children(root) {
                if child == tessera_node || child == container {
                    continue;
                }
                let name = format!("{prefab}/{}", self.node_name(child)?);
                if let Some(key) = self.props_key(&mut props, child, Some(name))? {
                    let (p, q, s) = self.local(child)?;
                    children.push(json!({"props": key, "ltw": affine(&trs(p, q, s))}));
                }
                walked.push(child);
            }
            let mut limited = Vec::new();
            for l in list(&b.fields, "_limitedGroups")? {
                let l = fields(l)?;
                let id = match self.target(wreck.0, get(l, "groupData")?)? {
                    Some(g) => match field(&self.behaviour(g)?.fields, "id") {
                        Some(Raw::Str(s)) => Some(s.clone()),
                        other => return Err(format!("_limitedGroups.groupData.id: {other:?}")),
                    },
                    None => None,
                };
                limited.push(json!([id, int(l, "maxQuantity")?]));
            }
            let generator = matrix_mul(&root_inverse, &self.ltw(tessera_node)?);
            let container_m = matrix_mul(&root_inverse, &self.ltw(container)?);
            let model = tessera::Model::new(self.tessera_generator(tessera)?)?;
            // The tiles in the prefab copy, in the generator's space.
            let generator_inverse = tessera::inverse(&generator);
            let mut fixed = Vec::new();
            self.scene_tiles(root, &root_inverse, &mut fixed)?;
            let mut scenes = Vec::new();
            for seed in 1..=SEEDS {
                // GenerateRandomPropsFromRoot on the other children of a fresh copy (its own
                // components' flags are fresh): the props it places, some with tiles.
                let mut random = MathRandom::new(seed);
                for child in &walked {
                    if let Some(Some(key)) = props.keys.get(child) {
                        for s in &props.lists[key] {
                            ignored.remove(&s.component);
                        }
                    }
                }
                let mut made = Vec::new();
                for child in &walked {
                    let Some(Some(key)) = props.keys.get(child).cloned() else { continue };
                    let (p, q, s) = self.local(*child)?;
                    self.walk_props(&props, &key, &trs(p, q, s), &container_m, &mut random, &mut ignored, &mut made)?;
                }
                let mut tiles = fixed.clone();
                for (prop, w) in made {
                    let (_, _, scale) = self.local(prop)?;
                    let l = lossy(&w);
                    let at = trs(crate::geometry::matrix_position(&w), crate::geometry::matrix_rotation(&w), std::array::from_fn(|i| scale[i] * l[i]));
                    self.scene_tiles(prop, &at, &mut tiles)?;
                }
                let mut scene = Vec::new();
                for (c, m) in tiles {
                    let t = self.tessera_tile(c)?;
                    scene.push(tessera::SceneTile { faces: t.faces, offsets: t.offsets, center: t.center, matrix: matrix_mul(&generator_inverse, &m) });
                }
                scenes.push(scene);
            }
            // Tessera from each seed with the scene's tiles, the next seed while it fails (the
            // seeds are independent: spread over the cores).
            let next = std::sync::atomic::AtomicUsize::new(0);
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(scenes.len().max(1));
            let mut results: Vec<(usize, Result<Vec<tessera::Instance>, String>)> = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut out = Vec::new();
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some(scene) = scenes.get(i) else { return out };
                                let seed = i as u32 + 1;
                                out.push((i, layout(&model, scene, seed, &prefab)));
                            }
                        })
                    })
                    .collect();
                workers.into_iter().flat_map(|w| w.join().expect("a Tessera worker panicked")).collect()
            });
            results.sort_by_key(|r| r.0);
            let mut layouts = Map::new();
            for (i, layout) in results {
                let mut out = Vec::new();
                for t in layout? {
                    let tile = &model.tiles[t.tile];
                    let node = self.behaviour(tile.id)?.game_object.ok_or("TesseraTile without a game object")?;
                    if let Some(key) = self.props_key(&mut props, node, None)? {
                        out.push(json!([key, v3(t.position), Value::Array(t.rotation.iter().map(|x| num(*x)).collect())]));
                    }
                }
                layouts.insert((i + 1).to_string(), Value::Array(out));
            }
            let (_, _, scale) = self.local(root)?;
            let triggers = self.triggers(root)?;
            generators.push(json!({
                "prefab": prefab,
                "scale": v3(scale),
                "generator": affine(&generator),
                "container": affine(&container_m),
                "children": children,
                "limitedGroups": limited,
                "sceneInstanceRadiusRatio": num(single(&b.fields, "_sceneInstanceRadiusRatio")?),
                "triggers": triggers,
                "layouts": layouts,
            }));
        }
        let props = self.props_json(&props);
        Ok(json!({"seeds": SEEDS, "generators": generators, "props": props}))
    }
}

/// Generation.Layouts for one seed: Tessera from the seed, the next one while it fails, up to
/// 50 seeds.
fn layout(model: &tessera::Model, scene: &[tessera::SceneTile], seed: u32, prefab: &str) -> Result<Vec<tessera::Instance>, String> {
    for current in seed..seed + 50 {
        if let Some(l) = tessera::generate(model, scene, current as i32)? {
            return Ok(l);
        }
    }
    Err(format!("{prefab} seed {seed}: Tessera failed on seeds {seed} to {}", seed + 49))
}

/// `pcds native-wrecks GAME_DIR OUT_FILE`: wrecks.json alone (read-game writes it too).
pub fn debug(args: &[String]) {
    let (Some(game), Some(out)) = (args.first(), args.get(1)) else {
        eprintln!("usage: pcds native-wrecks GAME_DIR OUT_FILE");
        std::process::exit(2);
    };
    let mut ctx = Ctx::open(Path::new(game)).expect("open game");
    if let Err(e) = ctx.assets() {
        eprintln!("native-wrecks: {e}");
        std::process::exit(1);
    }
    let started = std::time::Instant::now();
    match ctx.wrecks() {
        Ok(w) => {
            std::fs::write(out, serde_json::to_string_pretty(&w).unwrap()).expect("write wrecks");
            eprintln!("native-wrecks: written in {:?}", started.elapsed());
        }
        Err(e) => {
            eprintln!("native-wrecks: {e}");
            std::process::exit(1);
        }
    }
}

/// A type signature's text without assembly scopes (TypeRefs and TypeDefs name types alike).
fn sig_text(sig: &super::dotnet::TypeSig) -> String {
    super::netvars::type_text(sig)
}

/// Debug: the methods reachable from NS.CLASS.METHOD inside the given assemblies, with their IL
/// sizes. Virtual calls reach the overrides of the types created on the way (newobj) and of the
/// `+NS.CLASS` ones given (components Unity creates): rapid type analysis. `-TEXT` leaves out the
/// types whose name holds TEXT (branches a configuration never takes)
/// (pcds native-reach GAME_DIR NS.CLASS METHOD ASSEMBLY... [+NS.CLASS...] [-TEXT...]).
pub fn reach(args: &[String]) {
    use super::dotnet::{Member, TypeName, TypeSig};
    use std::collections::{BTreeMap, HashSet, VecDeque};
    let ctx = Ctx::open(Path::new(&args[0])).expect("open game");
    let asm = &ctx.layouts.asm;
    let scope: Vec<usize> = args[3..].iter().filter(|n| !n.starts_with('+') && !n.starts_with('-')).map(|n| asm.0.iter().position(|a| a.name == *n).unwrap_or_else(|| panic!("no assembly {n}"))).collect();
    let name_of = |a: usize, t: usize| TypeName { scope: Some(asm.0[a].name.clone()), namespace: asm.0[a].types()[t].namespace.clone(), name: asm.0[a].types()[t].name.clone() };
    let owner = |sig: &TypeSig| match sig {
        TypeSig::Class(t) | TypeSig::ValueType(t) | TypeSig::GenericInst { ty: t, .. } => Some(t.clone()),
        _ => None,
    };
    // The type, its bases or their interfaces are `target`.
    let is_a = |a: usize, t: usize, target: &str| -> bool {
        let mut cur = Some(name_of(a, t));
        for _ in 0..32 {
            let Some(c) = cur else { return false };
            if c.full() == target {
                return true;
            }
            let Some(d) = asm.def(&c) else { return false };
            if d.interfaces.iter().filter_map(owner).any(|i| i.full() == target) {
                return true;
            }
            cur = asm.base(&c);
        }
        false
    };
    // Overrides of a virtual (base type, name, parameter count) on a created type and its bases.
    let overrides = |a: usize, t: usize, site: &(String, String, usize)| -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        if !is_a(a, t, &site.0) {
            return out;
        }
        let mut cur = Some((a, t));
        while let Some((ca, ct)) = cur.filter(|(x, _)| scope.contains(x)) {
            for (j, x) in asm.0[ca].types()[ct].methods.iter().enumerate() {
                if (x.name == site.1 || x.name.ends_with(&format!(".{}", site.1))) && x.params.len() == site.2 && x.rva != 0 && x.flags & 0x40 != 0 {
                    out.push((ca, ct, j));
                }
            }
            cur = asm.base(&name_of(ca, ct)).and_then(|b| asm.resolve(&b));
        }
        out
    };
    let (ns, class) = args[1].rsplit_once('.').unwrap();
    let root = TypeName { scope: None, namespace: ns.into(), name: class.into() };
    let (ra, rt) = asm.resolve(&root).expect("no such type");
    let mut queue: VecDeque<(usize, usize, usize)> = asm.0[ra].types()[rt].methods.iter().enumerate().filter(|(_, m)| m.name == args[2]).map(|(i, _)| (ra, rt, i)).collect();
    let mut seen: HashSet<(usize, usize, usize)> = queue.iter().copied().collect();
    let mut created: Vec<(usize, usize)> = Vec::new();
    // What the runtime calls on our types from outside (foreach in LINQ and collections,
    // Dictionary and HashSet keys, sorting): always reached on created types.
    let mut sites: Vec<(String, String, usize)> = [
        ("System.Collections.IEnumerable", "GetEnumerator", 0),
        ("System.Collections.Generic.IEnumerable`1", "GetEnumerator", 0),
        ("System.Collections.IEnumerator", "MoveNext", 0),
        ("System.Collections.IEnumerator", "get_Current", 0),
        ("System.Collections.Generic.IEnumerator`1", "get_Current", 0),
        ("System.IDisposable", "Dispose", 0),
        ("System.Object", "Equals", 1),
        ("System.Object", "GetHashCode", 0),
        ("System.IEquatable`1", "Equals", 1),
        ("System.IComparable`1", "CompareTo", 1),
        ("System.Collections.Generic.IComparer`1", "Compare", 2),
        ("System.Collections.Generic.IEqualityComparer`1", "Equals", 2),
        ("System.Collections.Generic.IEqualityComparer`1", "GetHashCode", 1),
    ]
    .iter()
    .map(|(t, m, n)| (t.to_string(), m.to_string(), *n))
    .collect();
    let mut external: BTreeMap<String, usize> = BTreeMap::new();
    let skip: Vec<&str> = args[3..].iter().filter_map(|n| n.strip_prefix('-')).collect();
    let push = |key: (usize, usize, usize), seen: &mut HashSet<_>, queue: &mut VecDeque<_>| {
        let ty = &asm.0[key.0].types()[key.1];
        if skip.iter().any(|s| ty.name.contains(s) || ty.namespace.contains(s)) {
            return;
        }
        if seen.insert(key) {
            queue.push_back(key);
        }
    };
    let create = |at: (usize, usize), created: &mut Vec<(usize, usize)>, sites: &[(String, String, usize)], seen: &mut HashSet<_>, queue: &mut VecDeque<_>| {
        if created.contains(&at) {
            return;
        }
        created.push(at);
        for site in sites {
            for k in overrides(at.0, at.1, site) {
                push(k, seen, queue);
            }
        }
    };
    for extra in args[3..].iter().filter_map(|n| n.strip_prefix('+')) {
        let (ns, class) = extra.rsplit_once('.').unwrap();
        let at = asm.resolve(&TypeName { scope: None, namespace: ns.into(), name: class.into() }).unwrap_or_else(|| panic!("no type {extra}"));
        create(at, &mut created, &sites, &mut seen, &mut queue);
    }
    while let Some((a, t, m)) = queue.pop_front() {
        let def = &asm.0[a].types()[t].methods[m];
        // The type's static constructor runs before its first use.
        if let Some(c) = asm.0[a].types()[t].methods.iter().position(|x| x.name == ".cctor") {
            push((a, t, c), &mut seen, &mut queue);
        }
        for (code, member) in super::netvars::calls(&asm.0[a], asm.0[a].il(def).unwrap_or_default()) {
            let Member::Method { owner: o, name, params, .. } = member else { continue };
            let Some(on) = owner(&o) else { continue };
            let Some((ta, tt)) = asm.resolve(&on) else {
                *external.entry(format!("unresolved {}::{name}", on.full())).or_default() += 1;
                continue;
            };
            if matches!(code, 0x6F | 0xFE07) {
                let site = (on.full(), name.clone(), params.len());
                if !sites.contains(&site) {
                    for &(ca, ct) in &created {
                        for k in overrides(ca, ct, &site) {
                            push(k, &mut seen, &mut queue);
                        }
                    }
                    sites.push(site);
                }
            }
            if !scope.contains(&ta) {
                *external.entry(format!("{}::{name}", on.full())).or_default() += 1;
                continue;
            }
            let texts: Vec<String> = params.iter().map(sig_text).collect();
            // The method as declared, on the type or a base.
            let mut cur = Some((ta, tt));
            let mut target = None;
            while let Some((ca, ct)) = cur {
                if let Some(i) = asm.0[ca].types()[ct].methods.iter().position(|x| x.name == name && x.params.len() == params.len() && x.params.iter().map(|p| sig_text(&p.1)).collect::<Vec<_>>() == texts) {
                    target = Some((ca, ct, i));
                    break;
                }
                cur = asm.base(&name_of(ca, ct)).and_then(|b| asm.resolve(&b)).filter(|(x, _)| scope.contains(x));
            }
            // Signatures that do not compare as text (generic arguments): by name and arity.
            let target = target.or_else(|| asm.0[ta].types()[tt].methods.iter().position(|x| x.name == name && x.params.len() == params.len()).map(|i| (ta, tt, i)));
            let Some(target) = target else {
                *external.entry(format!("unresolved {}::{name}", on.full())).or_default() += 1;
                continue;
            };
            // Value types are made without newobj: count them as created once used.
            let value_type = matches!(&asm.0[target.0].types()[target.1].extends, Some(TypeSig::Class(b)) if b.full() == "System.ValueType");
            if code == 0x73 || value_type {
                create((target.0, target.1), &mut created, &sites, &mut seen, &mut queue);
            }
            push(target, &mut seen, &mut queue);
        }
    }
    let mut by_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut per_asm: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for &(a, t, m) in &seen {
        let def = &asm.0[a].types()[t].methods[m];
        let size = asm.0[a].il(def).map_or(0, |b| b.len());
        let e = by_type.entry(format!("{} {}", asm.0[a].name, name_of(a, t).full())).or_default();
        e.0 += 1;
        e.1 += size;
        let e = per_asm.entry(asm.0[a].name.clone()).or_default();
        e.0 += 1;
        e.1 += size;
    }
    for (t, (n, b)) in &by_type {
        println!("{b:7} {n:4} {t}");
    }
    // --il: the reached methods' IL too, by type.
    if args.iter().any(|a| a == "--il") {
        let mut all: Vec<_> = seen.iter().copied().collect();
        all.sort();
        let mut last = None;
        for (a, t, m) in all {
            let ty = &asm.0[a].types()[t];
            if last != Some((a, t)) {
                last = Some((a, t));
                println!("\n=== class {} : {:?}", name_of(a, t).full(), ty.extends.as_ref().map(sig_text));
                for f in &ty.fields {
                    println!("  field {}{} {}", if f.flags & 0x10 != 0 { "static " } else { "" }, f.name, sig_text(&f.sig));
                }
            }
            let def = &ty.methods[m];
            println!("--- {}({}) -> {}", def.name, def.params.iter().map(|p| format!("{} {}", sig_text(&p.1), p.0)).collect::<Vec<_>>().join(", "), sig_text(&def.ret));
            println!("  locals {}", asm.0[a].locals(def).iter().map(sig_text).collect::<Vec<_>>().join(", "));
            for line in super::netvars::disassemble(&asm.0[a], asm.0[a].il(def).unwrap_or_default()) {
                println!("  {line}");
            }
        }
    }
    for (a, (n, b)) in &per_asm {
        println!("total {a}: {n} methods, {b} IL bytes");
    }
    for (e, n) in &external {
        println!("external {n:4} {e}");
    }
}
