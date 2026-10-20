//! scenes/<planet>.json, the sections the server reads (GameData.WaterVolumes, GameData.OreVeins,
//! Generation.InstanceScenes), from every scene the planet can load: its scene and all its
//! sectors. The Wine-era extractor saw only the sectors loaded around it, so this is a superset
//! of what it dumped. Water volumes and veins are in the scenes' static state (what terraformation, story
//! events and loaded sectors move or show at runtime is the server's to apply, from
//! "activators"); instance scenes as they are once loaded (their Awakes run, before
//! InitSceneVariables), as they are in the running game.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::netdata::field;
use super::serial::Raw;
use super::tables::{single, Behaviour, Ctx};
use super::unity::{self, Id};

fn vector<const N: usize>(v: [f32; N]) -> Value {
    json!(v.map(single).to_vec())
}

/// The collider classes GetComponent<Collider> can return (with CharacterController,
/// WheelCollider and TerrainCollider, which `collider` refuses).
const COLLIDERS: [i32; 7] = [unity::MESH_COLLIDER, unity::BOX_COLLIDER, unity::SPHERE_COLLIDER, unity::CAPSULE_COLLIDER, 143, 146, 154];

impl Ctx {
    /// GameData.Fields of a behaviour.
    fn written(&mut self, id: Id) -> Result<Value, String> {
        let b = self.behaviour(id)?;
        Ok(Value::Object(self.fields(id.0, &b.fields, 0).map_err(|e| format!("{}: {e}", b.class))?))
    }

    /// The SpaceCraft MonoBehaviours of a class on one game object (GetComponents order).
    fn on(&mut self, node: Id, class: &str) -> Result<Vec<Id>, String> {
        let components = self.graph.nodes.get(&node).ok_or_else(|| format!("{node:?}: not a game object of a file read"))?.components.clone();
        self.of_class(components.into_iter().filter(|(_, k)| *k == unity::MONO_BEHAVIOUR).map(|(c, _)| c).collect(), class)
    }

    /// The SpaceCraft MonoBehaviours of a class in a file, in file order.
    fn all_of(&mut self, file: usize, class: &str) -> Result<Vec<Id>, String> {
        let behaviours: Vec<Id> = self.data.files[file].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| Id(file, o.path)).collect();
        self.of_class(behaviours, class)
    }

    fn of_class(&mut self, behaviours: Vec<Id>, class: &str) -> Result<Vec<Id>, String> {
        let mut out = Vec::new();
        for b in behaviours {
            if self.graph.script(&mut self.data, b)?.is_some_and(|s| s.0 == "SpaceCraft" && s.1 == class) {
                out.push(b);
            }
        }
        Ok(out)
    }

    fn name(&self, node: Id) -> String {
        self.graph.nodes.get(&node).map(|n| n.name.clone()).unwrap_or_else(|| format!("{node:?}"))
    }

    /// The scene files a server on the planet has loaded or can load: BootScene, GameMainScene,
    /// the planet's scene, and every sector a SectorEnter of those can load (Sector.LoadSector,
    /// called only from SectorEnter.OnTriggerEnter, loads the scene named as the sector's game
    /// object), sectors entered from sectors included.
    pub(super) fn planet_files(&mut self, scene: &str) -> Result<Vec<usize>, String> {
        let mut files = Vec::new();
        let mut queue: Vec<String> = ["BootScene", "GameMainScene", scene].map(String::from).to_vec();
        let mut i = 0;
        while i < queue.len() {
            let f = self.scene_file(&queue[i])?;
            i += 1;
            if files.contains(&f) {
                continue;
            }
            files.push(f);
            self.graph_file(f)?;
            for enter in self.all_of(f, "SectorEnter")? {
                let b = self.behaviour(enter)?;
                let sector = reference(self, f, &b.fields, "sector")?.ok_or_else(|| format!("{}: a SectorEnter without a sector", self.data.files[f].name))?;
                let node = self.behaviour(sector)?.game_object.ok_or("a Sector without a game object")?;
                let name = self.name(node);
                if !queue.contains(&name) {
                    queue.push(name);
                }
            }
        }
        Ok(files)
    }

    /// GameData.Activators: HideDependingOnTerraStage and WorldUnitPositioning on the object and
    /// its parents.
    fn activators(&mut self, node: Id) -> Result<Value, String> {
        let mut list = Vec::new();
        let mut n = Some(node);
        while let Some(id) = n {
            for class in ["HideDependingOnTerraStage", "WorldUnitPositioning"] {
                for c in self.on(id, class)? {
                    list.push(json!({"type": class, "fields": self.written(c)?}));
                }
            }
            n = self.graph.parent(id);
        }
        Ok(Value::Array(list))
    }

    /// A collider component: its class, game object, enabled flag and shape.
    fn collider(&mut self, id: Id) -> Result<(i32, unity::Collider), String> {
        let class = self.data.class_of(id).ok_or_else(|| format!("{id:?}: no such object"))?;
        if ![unity::MESH_COLLIDER, unity::BOX_COLLIDER, unity::SPHERE_COLLIDER, unity::CAPSULE_COLLIDER].contains(&class) {
            return Err(format!("{id:?}: collider class {class} is not read"));
        }
        let c = self.data.reader(id).and_then(|mut r| unity::read_collider(class, &mut r)).ok_or_else(|| format!("{id:?}: unreadable collider"))?;
        Ok((class, c))
    }

    fn pose(&self, node: Id) -> Result<Map<String, Value>, String> {
        let (p, r, s) = self.graph.world(node)?;
        let mut m = Map::new();
        m.insert("position".into(), vector(p));
        m.insert("rotation".into(), vector(r));
        m.insert("scale".into(), vector(s));
        Ok(m)
    }

    /// GameData.WaterVolumes, one volume.
    fn water_volume(&mut self, id: Id) -> Result<Value, String> {
        let b = self.behaviour(id)?;
        let node = attached(&b)?;
        let ignore = match field(&b.fields, "ignoreForEnergyDetection") {
            Some(Raw::Bool(v)) => *v,
            other => return Err(format!("WaterVolume.ignoreForEnergyDetection: {other:?}")),
        };
        // GetComponentInChildren<BoxCollider>(true): depth first, inactive ones too.
        let mut found = None;
        for n in self.graph.descendants(node) {
            let components = self.graph.nodes.get(&n).map(|x| x.components.clone()).unwrap_or_default();
            if let Some((c, _)) = components.into_iter().find(|(_, k)| *k == unity::BOX_COLLIDER) {
                found = Some((c, n));
                break;
            }
        }
        let shape = match found {
            None => Value::Null,
            Some((c, n)) => {
                let (_, col) = self.collider(c)?;
                let unity::Shape::Box { center, size } = col.shape else { unreachable!() };
                let mut m = Map::new();
                m.insert("center".into(), vector(center));
                m.insert("size".into(), vector(size));
                m.extend(self.pose(n)?);
                Value::Object(m)
            }
        };
        let positioning = match self.on(node, "WorldUnitPositioning")?.first() {
            Some(p) => self.written(*p)?,
            None => Value::Null,
        };
        Ok(json!({
            "ignoreForEnergyDetection": ignore,
            "active": self.graph.active_in_hierarchy(node),
            "root": self.pose(node)?,
            "box": shape,
            "positioning": positioning,
            "activators": self.activators(node)?,
            "storyInterpolation": self.story_interpolation(node)?,
        }))
    }

    /// SceneInterpolationFromStoryEvent on the volume or a parent: that object starts at its
    /// scene position plus _startingOffset and moves to its scene position as the
    /// SceneInterpolationHandler progress of its WorldUniqueId goes from 0 to 1 (eased with
    /// InOutQuad when _ease). The scene positions above are the end ones.
    fn story_interpolation(&mut self, node: Id) -> Result<Value, String> {
        let mut at = Some(node);
        while let Some(n) = at {
            if let Some(s) = self.on(n, "SceneInterpolationFromStoryEvent")?.first().copied() {
                let b = self.behaviour(s)?;
                let offset = match field(&b.fields, "_startingOffset") {
                    Some(Raw::Json(v)) => v.clone(),
                    other => return Err(format!("SceneInterpolationFromStoryEvent._startingOffset: {other:?}")),
                };
                let ease = matches!(field(&b.fields, "_ease"), Some(Raw::Bool(true)));
                let id = self.on(n, "WorldUniqueId")?.first().copied().ok_or("a SceneInterpolationFromStoryEvent without a WorldUniqueId")?;
                let unique = match field(&self.behaviour(id)?.fields, "worldUniqueId") {
                    Some(Raw::Int(v)) => *v,
                    other => return Err(format!("WorldUniqueId.worldUniqueId: {other:?}")),
                };
                return Ok(json!({"worldObject": unique, "offset": offset, "ease": ease}));
            }
            at = self.graph.parent(n);
        }
        Ok(Value::Null)
    }

    /// GameData.ToxicAreas, one area: where it stands (its id is made of its position), whether it
    /// is shown, and its fields (the time a gauge lasts in it, the scene objects that make it toxic).
    fn toxic_area(&mut self, id: Id) -> Result<Value, String> {
        let b = self.behaviour(id)?;
        let node = attached(&b)?;
        Ok(json!({
            "position": vector(self.graph.world(node)?.0),
            "active": self.graph.active_in_hierarchy(node),
            "activators": self.activators(node)?,
            "fields": Value::Object(self.fields(id.0, &b.fields, 0).map_err(|e| format!("{}: {e}", b.class))?),
        }))
    }

    /// GameData.OreVeins, one vein.
    fn ore_vein(&mut self, id: Id) -> Result<Value, String> {
        let b = self.behaviour(id)?;
        let node = attached(&b)?;
        let unique = match self.on(node, "WorldUniqueId")?.first() {
            // WorldUniqueId.GetWorldUniqueId returns the field.
            Some(u) => match field(&self.behaviour(*u)?.fields, "worldUniqueId") {
                Some(Raw::Int(v)) => *v,
                other => return Err(format!("WorldUniqueId.worldUniqueId: {other:?}")),
            },
            None => 0,
        };
        let active = self.graph.active_in_hierarchy(node);
        let components = self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default();
        let mut colliders = Vec::new();
        for (c, class) in components {
            if !COLLIDERS.contains(&class) {
                continue;
            }
            let (_, col) = self.collider(c)?;
            let (p, r, s) = self.graph.world(node)?;
            let unity::Shape::Box { center, size } = col.shape else { return Err(format!("{}: ore vein collider class {class}: only boxes are read", self.name(node))) };
            // Collider.bounds: the world AABB; a collider not in the physics scene (disabled or
            // inactive) has none: zero size at its transform.
            let (bounds_center, bounds_size) = if col.enabled && active {
                let half = [0, 1, 2].map(|i| size[i] * s[i].abs() / 2.0);
                let c = crate::geometry::rotate(r, [0, 1, 2].map(|i| center[i] * s[i]));
                let axes = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].map(|a| crate::geometry::rotate(r, a));
                let extent = [0, 1, 2].map(|i| (0..3).map(|j| axes[j][i].abs() * half[j]).sum::<f32>());
                ([0, 1, 2].map(|i| p[i] + c[i]), extent.map(|e| 2.0 * e))
            } else {
                (p, [0.0; 3])
            };
            let mut shape = Map::new();
            shape.insert("center".into(), vector(center));
            shape.insert("size".into(), vector(size));
            shape.extend(self.pose(node)?);
            colliders.push(json!({"enabled": col.enabled, "boundsCenter": vector(bounds_center), "boundsSize": vector(bounds_size), "box": shape}));
        }
        Ok(json!({
            "id": unique,
            "active": active,
            "activators": self.activators(node)?,
            "fields": Value::Object(self.fields(id.0, &b.fields, 0).map_err(|e| format!("{}: {e}", b.class))?),
            "colliders": colliders,
        }))
    }
}

fn attached(b: &Behaviour) -> Result<Id, String> {
    b.game_object.ok_or_else(|| format!("{} without a game object", b.class))
}

/// A reference field of a read object, resolved (None for null).
fn reference(ctx: &mut Ctx, file: usize, fields: &[(String, Raw)], name: &str) -> Result<Option<Id>, String> {
    match field(fields, name) {
        Some(Raw::Ref(p, _)) => ctx.data.resolve(file, *p),
        other => Err(format!("{name}: {other:?}")),
    }
}

fn list<'a>(fields: &'a [(String, Raw)], name: &str) -> Result<&'a [Raw], String> {
    match field(fields, name) {
        Some(Raw::List(items)) => Ok(items),
        other => Err(format!("{name}: {other:?}")),
    }
}

/// Generation.InstanceScene's numbering: game objects with their parents first, with their
/// activeSelf once the scene has loaded.
struct Nodes {
    ids: HashMap<Id, usize>,
    rows: Vec<Value>,
    active: HashMap<Id, bool>,
    /// Objects whose activeSelf after load depends on the order of the Awakes.
    unsure: std::collections::HashSet<Id>,
}

impl Nodes {
    fn node(&mut self, ctx: &Ctx, go: Option<Id>) -> Result<i64, String> {
        let Some(go) = go else { return Ok(-1) };
        if let Some(id) = self.ids.get(&go) {
            return Ok(*id as i64);
        }
        let active = *self.active.get(&go).ok_or_else(|| format!("{} ({go:?}) is not an object of the instance scene", ctx.name(go)))?;
        let parent = self.node(ctx, ctx.graph.parent(go))?;
        let id = self.rows.len();
        self.ids.insert(go, id);
        self.rows.push(json!([parent, active, ctx.name(go)]));
        Ok(id as i64)
    }

    fn in_hierarchy(&self, ctx: &Ctx, node: Option<Id>) -> Result<bool, String> {
        shown(&ctx.graph, &self.active, node)
    }
}

/// Transform.position and .rotation once the hierarchy's root is at `root` (HandleDataAfterLoad
/// moves an instance scene's roots), composed as Unity does: from the object up, each parent's
/// local scale, rotation (Quaternion * Vector3) and position in turn; mostly bit for bit.
fn placed(graph: &super::graph::Graph, node: Id, root: [f32; 3]) -> Result<([f32; 3], [f32; 4]), String> {
    let local = |n: Id| graph.xform(n).ok_or_else(|| format!("{n:?}: not a game object of a file read"));
    let x = local(node)?;
    let (mut p, mut q) = (x.position, x.rotation);
    let mut parent = graph.parent(node);
    if parent.is_none() {
        p = root;
    }
    while let Some(n) = parent {
        let t = local(n)?;
        let r = mul_vector(t.rotation, [0, 1, 2].map(|i| p[i] * t.scale[i]));
        parent = graph.parent(n);
        let base = if parent.is_some() { t.position } else { root };
        p = [0, 1, 2].map(|i| r[i] + base[i]);
        q = mul_quaternion(t.rotation, q);
    }
    Ok((p, q))
}

/// UnityEngine.Quaternion * Quaternion.
fn mul_quaternion(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let ([ax, ay, az, aw], [bx, by, bz, bw]) = (a, b);
    [aw * bx + ax * bw + ay * bz - az * by, aw * by + ay * bw + az * bx - ax * bz, aw * bz + az * bw + ax * by - ay * bx, aw * bw - ax * bx - ay * by - az * bz]
}

/// UnityEngine.Quaternion * Vector3.
fn mul_vector(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let (x2, y2, z2) = (x * 2.0, y * 2.0, z * 2.0);
    let (xx, yy, zz, xy, xz, yz) = (x * x2, y * y2, z * z2, x * y2, x * z2, y * z2);
    let (wx, wy, wz) = (w * x2, w * y2, w * z2);
    [
        (1.0 - (yy + zz)) * v[0] + (xy - wz) * v[1] + (xz + wy) * v[2],
        (xy + wz) * v[0] + (1.0 - (xx + zz)) * v[1] + (yz - wx) * v[2],
        (xz - wy) * v[0] + (yz + wx) * v[1] + (1.0 - (xx + yy)) * v[2],
    ]
}

/// activeInHierarchy with these activeSelf values.
fn shown(graph: &super::graph::Graph, active: &HashMap<Id, bool>, mut node: Option<Id>) -> Result<bool, String> {
    while let Some(n) = node {
        if !*active.get(&n).ok_or_else(|| format!("{n:?} is not an object of the scene"))? {
            return Ok(false);
        }
        node = graph.parent(n);
    }
    Ok(true)
}

impl Ctx {
    /// activeSelf of a scene's objects once it has loaded: the Awake of each component active
    /// then, or activated by another's, that sets objects active: WorldVariationObject
    /// (DisableWorldRandomObject: the whole object hidden, else whenEnabled hidden and
    /// whenDisabled shown) and HideDependingOnTerraStage (hideIfLesser: hidden until its
    /// coroutine looks at the stage, after InitSceneVariables).
    fn loaded(&mut self, file: usize) -> Result<Nodes, String> {
        let active = self.graph.nodes.iter().filter(|(id, _)| id.0 == file).map(|(id, n)| (*id, n.active)).collect();
        let mut nodes = Nodes { ids: HashMap::new(), rows: Vec::new(), active, unsure: Default::default() };
        let bool_field = |b: &Behaviour, name: &str| match field(&b.fields, name) {
            Some(Raw::Bool(v)) => Ok(*v),
            other => Err(format!("{}.{name}: {other:?}", b.class)),
        };
        // (object, what its Awake hides, what it shows); hiding its own object is self-contained.
        let mut wakers: Vec<(Id, Vec<Id>, Vec<Id>)> = Vec::new();
        let mut ids = self.all_of(file, "WorldVariationObject")?;
        ids.extend(self.all_of(file, "HideDependingOnTerraStage")?);
        ids.sort();
        for c in ids {
            let b = self.behaviour(c)?;
            let node = attached(&b)?;
            if b.class == "HideDependingOnTerraStage" {
                if bool_field(&b, "hideIfGreater")? {
                    return Err(format!("{}: HideDependingOnTerraStage.hideIfGreater on {}: shown at load depending on terraformation", self.data.files[file].name, self.name(node)));
                }
                if bool_field(&b, "hideIfLesser")? {
                    wakers.push((node, vec![node], vec![]));
                }
            } else if bool_field(&b, "wholeGameObject")? {
                wakers.push((node, vec![node], vec![]));
            } else {
                let (on, off) = (reference(self, file, &b.fields, "whenEnabled")?, reference(self, file, &b.fields, "whenDisabled")?);
                wakers.push((node, on.into_iter().collect(), off.into_iter().collect()));
            }
        }
        // Unity does not order these Awakes: settled in file order, and in reverse to find what
        // depends on the order (an error if the dump uses it).
        for (_, hide, show) in &wakers {
            if let Some(n) = hide.iter().chain(show).find(|n| !nodes.active.contains_key(n)) {
                return Err(format!("{}: an Awake shows or hides {} ({n:?}), outside the scene", self.data.files[file].name, self.name(*n)));
            }
        }
        let settle = |order: Vec<usize>| -> Result<HashMap<Id, bool>, String> {
            let mut active = nodes.active.clone();
            let mut woke = vec![false; wakers.len()];
            'wake: loop {
                for &i in &order {
                    if !woke[i] && shown(&self.graph, &active, Some(wakers[i].0))? {
                        woke[i] = true;
                        active.extend(wakers[i].1.iter().map(|n| (*n, false)).chain(wakers[i].2.iter().map(|n| (*n, true))));
                        continue 'wake;
                    }
                }
                return Ok(active);
            }
        };
        let (forward, reverse) = (settle((0..wakers.len()).collect())?, settle((0..wakers.len()).rev().collect())?);
        nodes.unsure = forward.iter().filter(|(n, v)| reverse[*n] != **v).map(|(n, _)| *n).collect();
        nodes.active = forward;
        Ok(nodes)
    }
}

impl Ctx {
    /// Generation.InstanceScene: an instance scene with its roots moved where the game moves
    /// them (WorldInstanceHandler.HandleDataAfterLoad), before InitSceneVariables.
    fn instance_scene(&mut self, file: usize, target: [f32; 3]) -> Result<Value, String> {
        let name = self.data.files[file].name.clone();
        let mut nodes = self.loaded(file)?;
        // roots.Select(GetComponentInChildren<WorldInstanceScene>).First: active objects only.
        let scenes: Vec<Id> = self.all_of(file, "WorldInstanceScene")?;
        let mut active = Vec::new();
        for s in scenes {
            let b = self.behaviour(s)?;
            let node = attached(&b)?;
            if nodes.in_hierarchy(self, Some(node))? {
                active.push((s, node, b));
            }
        }
        let (scene, root, b) = match active.len() {
            1 => active.pop().unwrap(),
            n => return Err(format!("{name}: {n} active WorldInstanceScene components (one is read)")),
        };
        let mut object_ids: HashMap<Id, usize> = HashMap::new();
        let mut objects: Vec<Value> = Vec::new();
        let mut toggle = |ctx: &mut Ctx, nodes: &mut Nodes, item: &Raw| -> Result<usize, String> {
            let Raw::Ref(p, _) = item else { return Err(format!("{name}: a world variation object is {item:?}")) };
            let o = ctx.data.resolve(scene.0, *p)?.ok_or_else(|| format!("{name}: a null world variation object"))?;
            if let Some(id) = object_ids.get(&o) {
                return Ok(*id);
            }
            let id = objects.len();
            object_ids.insert(o, id);
            let v = ctx.behaviour(o)?;
            let go = attached(&v)?;
            let whole = match field(&v.fields, "wholeGameObject") {
                Some(Raw::Bool(w)) => *w,
                other => return Err(format!("WorldVariationObject.wholeGameObject: {other:?}")),
            };
            let node = nodes.node(ctx, Some(go))?;
            let enabled = reference(ctx, o.0, &v.fields, "whenEnabled")?;
            let enabled = nodes.node(ctx, enabled)?;
            let disabled = reference(ctx, o.0, &v.fields, "whenDisabled")?;
            let disabled = nodes.node(ctx, disabled)?;
            objects.push(json!({"node": node, "whole": whole, "whenEnabled": enabled, "whenDisabled": disabled}));
            Ok(id)
        };
        let mut variations = Vec::new();
        for v in list(&b.fields, "worldVariations")?.to_vec() {
            let variation = match v {
                Raw::Ref(p, _) => self.data.resolve(scene.0, p)?.ok_or_else(|| format!("{name}: a null world variation"))?,
                other => return Err(format!("{name}: a world variation is {other:?}")),
            };
            let f = &self.behaviour(variation)?.fields;
            let mut every = Vec::new();
            for o in list(f, "objectsToEnableEveryTime")? {
                every.push(toggle(self, &mut nodes, o)?);
            }
            let mut random = Vec::new();
            for i in 1..=6 {
                let range = match field(f, &format!("numberOfThoseObjects{i}")) {
                    Some(Raw::Json(Value::Array(a))) if a.len() == 2 && a.iter().all(Value::is_number) => [0, 1].map(|j| a[j].as_f64().expect("a number")),
                    other => return Err(format!("WorldVariation.numberOfThoseObjects{i}: {other:?}")),
                };
                let mut ids = Vec::new();
                for o in list(f, &format!("randomObjects{i}"))? {
                    ids.push(toggle(self, &mut nodes, o)?);
                }
                // [min, max (exclusive), objects] as RandomizeObjectApparition rounds them.
                random.push(json!([range[0].round_ties_even() as i64, range[1].round_ties_even() as i64 + 1, ids]));
            }
            variations.push(json!({"everyTime": every, "random": random}));
        }
        // Every RandomizePropData under the scene component, active or not, in walk order.
        let mut props: Vec<(Id, Id)> = Vec::new();
        for n in self.graph.descendants(root) {
            if let Some(p) = self.on(n, "RandomizePropData")?.first() {
                props.push((*p, n));
            }
        }
        let root_id = nodes.node(self, Some(root))?;
        let mut wrecks = Vec::new();
        for e in list(&b.fields, "wreckEntries")?.to_vec() {
            let Raw::Ref(p, _) = e else { return Err(format!("{name}: a wreck entry is {e:?}")) };
            let go = self.data.resolve(scene.0, p)?.ok_or_else(|| format!("{name}: a null wreck entry"))?;
            let id = nodes.node(self, Some(go))?;
            let (p, r) = placed(&self.graph, go, target)?;
            wrecks.push(json!({"node": id, "position": vector(p), "rotation": vector(r)}));
        }
        let mut by_prefab: HashMap<Id, Vec<String>> = HashMap::new();
        for (gid, prefab) in &self.groups {
            if let Some(p) = prefab {
                by_prefab.entry(*p).or_default().push(gid.clone());
            }
        }
        let prop_ids: Vec<Id> = props.iter().map(|p| p.0).collect();
        let mut rows = Vec::new();
        for (p, n) in props {
            let r = self.behaviour(p)?;
            let mut chances = Vec::new();
            for d in list(&r.fields, "props")? {
                let Raw::Fields(_, f) = d else { return Err(format!("RandomizePropData.props: {d:?}")) };
                let chance = match field(f, "chancePourcentage") {
                    Some(Raw::Int(c)) => *c,
                    other => return Err(format!("RandomData.chancePourcentage: {other:?}")),
                };
                // GroupsHandler.GetGroupsViaGameObject: the group whose prefab it is.
                let group = match reference(self, p.0, f, "prop")? {
                    None => Value::Null,
                    Some(prefab) => match by_prefab.get(&prefab).map(Vec::as_slice) {
                        None => Value::Null,
                        Some([g]) => json!(g),
                        Some(many) => return Err(format!("{name}: prop prefab {prefab:?} belongs to several groups {many:?}")),
                    },
                };
                chances.push(json!([chance, group]));
            }
            let mut ignore = Vec::new();
            for x in list(&r.fields, "ignoreIfGenerated")? {
                let Raw::Ref(q, _) = x else { return Err(format!("RandomizePropData.ignoreIfGenerated: {x:?}")) };
                if let Some(i) = self.data.resolve(p.0, *q)?.and_then(|q| prop_ids.iter().position(|a| *a == q)) {
                    ignore.push(i);
                }
            }
            let id = nodes.node(self, Some(n))?;
            let (p, r) = placed(&self.graph, n, target)?;
            rows.push(json!([id, vector(p), vector(r), chances, ignore]));
        }
        // GameObjects.FindRecursively(instance.transform.root, "PreventBuilding"), its first Collider.
        let mut top = root;
        while let Some(p) = self.graph.parent(top) {
            top = p;
        }
        let prevent = match self.find_recursively(top, "PreventBuilding") {
            None => Value::Null,
            Some(n) => {
                let components = self.graph.nodes.get(&n).map(|x| x.components.clone()).unwrap_or_default();
                match components.into_iter().find(|(_, k)| COLLIDERS.contains(k)) {
                    None => Value::Null,
                    Some((c, _)) => {
                        let active = nodes.in_hierarchy(self, Some(n))?;
                        let pose = placed(&self.graph, n, target)?;
                        self.prevent_building(c, n, pose, active).map_err(|e| format!("{name}: PreventBuilding: {e}"))?
                    }
                }
            }
        };
        if let Some(n) = nodes.ids.keys().find(|n| nodes.unsure.contains(n)) {
            return Err(format!("{}: active or not after load depending on the order of the Awakes", self.name(*n)));
        }
        Ok(json!({
            "root": root_id,
            "variations": variations,
            "objects": objects,
            "wreckEntries": wrecks,
            "props": rows,
            "preventBuilding": prevent,
            "nodes": nodes.rows,
        }))
    }

    /// GameObjects.FindRecursively: a direct child by name (Transform.Find), else the first match
    /// below each child in turn.
    fn find_recursively(&self, node: Id, name: &str) -> Option<Id> {
        let children = self.graph.children(node);
        children.iter().find(|c| self.graph.nodes.get(*c).is_some_and(|n| n.name == name)).copied().or_else(|| children.iter().find_map(|c| self.find_recursively(*c, name)))
    }

    /// Generation.Shape of the PreventBuilding collider, in world space: an enabled, active box
    /// (the only kind the server tests points against; 2.103 has no other).
    fn prevent_building(&mut self, id: Id, node: Id, (position, rotation): ([f32; 3], [f32; 4]), active: bool) -> Result<Value, String> {
        let (class, c) = self.collider(id)?;
        let unity::Shape::Box { center, size } = c.shape else { return Err(format!("a {} (only a BoxCollider is read)", unity::class_name(class).unwrap_or("collider"))) };
        if !(c.enabled && active) {
            return Err("an inactive or disabled collider (not read)".into());
        }
        let (_, _, s) = self.graph.world(node)?;
        let mut m = Map::new();
        m.insert("type".into(), json!(unity::class_name(class)));
        m.insert("active".into(), json!(true));
        m.insert("position".into(), vector(position));
        m.insert("rotation".into(), vector(rotation));
        m.insert("scale".into(), vector(s));
        m.insert("center".into(), vector(center));
        m.insert("size".into(), vector(size));
        Ok(Value::Object(m))
    }

    /// Where WorldInstanceHandler.HandleDataAfterLoad moves an instance scene's roots: the active
    /// game object "WorldInstancePositionToInstantiate" in the loaded scenes (GameObject.Find),
    /// else the WorldInstanceHandler's positionToInstantiate.
    fn instance_target(&mut self, files: &[usize]) -> Result<[f32; 3], String> {
        let named: Vec<Id> = self.graph.nodes.iter().filter(|(id, n)| files.contains(&id.0) && n.name == "WorldInstancePositionToInstantiate").map(|(id, _)| *id).collect();
        let named: Vec<Id> = named.into_iter().filter(|n| self.graph.active_in_hierarchy(*n)).collect();
        if let [n] = named[..] {
            return Ok(self.graph.world(n)?.0);
        }
        if named.len() > 1 {
            return Err(format!("{} active WorldInstancePositionToInstantiate objects", named.len()));
        }
        let mut handlers = Vec::new();
        for f in files {
            for h in self.all_of(*f, "WorldInstanceHandler")? {
                let b = self.behaviour(h)?;
                let node = attached(&b)?;
                if self.graph.active_in_hierarchy(node) {
                    handlers.push((h, b));
                }
            }
        }
        let [(h, b)] = &handlers[..] else { return Err(format!("{} active WorldInstanceHandler components (one is read)", handlers.len())) };
        let target = reference(self, h.0, &b.fields, "positionToInstantiate")?.ok_or("WorldInstanceHandler.positionToInstantiate is null")?;
        self.graph_file(target.0)?;
        Ok(self.graph.world(target)?.0)
    }

    /// scenes/<planet>.json for a planet (its PlanetData asset).
    pub fn planet_scenes(&mut self, assets: &Map<String, Value>, planet: &Value) -> Result<Value, String> {
        let scene = planet["sceneName"].as_str().ok_or("PlanetData.sceneName")?;
        let files = self.planet_files(scene)?;
        let mut water = Vec::new();
        let mut veins = Vec::new();
        let mut toxic = Vec::new();
        for f in &files {
            for v in self.all_of(*f, "WaterVolume")? {
                water.push(self.water_volume(v)?);
            }
            for v in self.all_of(*f, "MachineGenerationGroupVein")? {
                veins.push(self.ore_vein(v)?);
            }
            for v in self.all_of(*f, "ToxicArea")? {
                toxic.push(self.toxic_area(v)?);
            }
        }
        let mut instances = Map::new();
        if let Some(data) = planet["instanceScenesData"].as_str() {
            let data = assets.get(data).ok_or_else(|| format!("no asset {data}"))?;
            let names = data["instanceSceneNames"].as_array().ok_or("WorldInstanceGeneratorData.instanceSceneNames")?;
            for n in names {
                let n = n.as_str().ok_or("an instance scene name")?;
                if !instances.contains_key(n) {
                    let file = self.scene_file(n)?;
                    self.graph_file(file)?;
                    let target = self.instance_target(&[&files[..], &[file]].concat())?;
                    let dump = self.instance_scene(file, target).map_err(|e| format!("instance scene {n}: {e}"))?;
                    instances.insert(n.to_string(), dump);
                }
            }
        }
        eprintln!("read-game: {scene}: {} scene files, {} water volumes, {} ore veins, {} toxic areas, {} instance scenes", files.len(), water.len(), veins.len(), toxic.len(), instances.len());
        let mut dump = json!({"waterVolumes": water, "oreVeins": veins, "instanceScenes": instances});
        // Only the planets that have toxic areas carry the section: the others' dumps stay as they were.
        if !toxic.is_empty() {
            dump["toxicAreas"] = Value::Array(toxic);
        }
        Ok(dump)
    }
}

/// Debug: the script classes of every planet's scene files, counted per planet
/// (pcds native-classes GAME_DIR PLANET_SCENE...; a class a DLC planet has and no free planet has
/// is a feature the server may be missing).
pub fn classes(args: &[String]) {
    let mut ctx = Ctx::open(std::path::Path::new(&args[0])).expect("open game");
    let mut table: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, scene) in args[1..].iter().enumerate() {
        let files = ctx.planet_files(scene).expect("planet files");
        for f in files {
            let all: Vec<Id> = ctx.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| Id(f, o.path)).collect();
            for b in all {
                if let Some((ns, class, _)) = ctx.graph.script(&mut ctx.data, b).expect("script").cloned() {
                    table.entry(format!("{ns}.{class}")).or_insert_with(|| vec![0; args.len() - 1])[i] += 1;
                }
            }
        }
    }
    for (class, counts) in table {
        println!("{class}\t{}", counts.iter().map(|c| c.to_string()).collect::<Vec<_>>().join("\t"));
    }
}

/// Debug: the fields of every instance of a script class in each planet's scene files, as
/// JSON lines (pcds native-fields GAME_DIR CLASS PLANET_SCENE...), to compare a class between
/// planets.
pub fn fields_of(args: &[String]) {
    let mut ctx = Ctx::open(std::path::Path::new(&args[0])).expect("open game");
    for scene in &args[2..] {
        for f in ctx.planet_files(scene).expect("planet files") {
            for b in ctx.all_of(f, &args[1]).expect("instances") {
                let fields = ctx.written(b).expect("fields");
                let at = ctx.behaviour(b).ok().and_then(|b| b.game_object).map(|n| ctx.name(n)).unwrap_or_default();
                println!("{scene}\t{}\t{at}\t{fields}", ctx.data.files[f].name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tables::Ctx;

    /// What the extractor dumped from the game (2.103): the instance scene placed, and its
    /// WorldVariationObjects' Awakes run.
    #[test]
    fn instance_scenes_are_placed_and_woken_as_in_the_game() {
        let Some(dir) = crate::testkit::game_data() else { return };
        let mut ctx = Ctx::open(dir.parent().unwrap()).unwrap();
        ctx.assets().unwrap();
        let (main, file) = (ctx.scene_file("GameMainScene").unwrap(), ctx.scene_file("Instance-HugeTrees").unwrap());
        ctx.graph_file(main).unwrap();
        ctx.graph_file(file).unwrap();
        let target = ctx.instance_target(&[main, file]).unwrap();
        let scene = ctx.instance_scene(file, target).unwrap();
        assert_eq!(scene["wreckEntries"][0]["position"], json!([1239.3069, 67.828445, 4634.242]));
        assert_eq!((&scene["nodes"][13], &scene["nodes"][14]), (&json!([12, false, "WhenEnabled"]), &json!([12, true, "WhenDisabled"])));
    }
}
