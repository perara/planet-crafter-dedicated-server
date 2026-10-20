//! GameData panels, for Panel.GetContingousPanels: each building's habitat panels relative to
//! the building with the box colliders the ray hits, and per panel look (BuildPanelSubType) the
//! bounds BoundsHelper.GetBoundsOfChildren and Panel.GetBoundsOfPanel give a panel holding it.

use serde_json::{json, Map, Value};

use super::netdata::field;
use super::serial::Raw;
use super::tables::{single, Ctx};
use super::unity::{self, Id};
use crate::geometry::{conj, mul, rotate};

const MESH_RENDERER: i32 = 23;
const PARTICLE_SYSTEM_RENDERER: i32 = 199;

fn v3(v: [f32; 3]) -> Value {
    json!(v.map(single).to_vec())
}

impl Ctx {
    /// A node's position, rotation and lossy scale in its root's frame, the root standing at the
    /// origin unrotated (as InstantiateWorldObject and the look instance put it) with its scale.
    pub(super) fn in_root(&self, root: Id, node: Id) -> Result<([f32; 3], [f32; 4], [f32; 3]), String> {
        let (rp, rr, _) = self.graph.world(root)?;
        let (p, r, s) = self.graph.world(node)?;
        let inverse = conj(rr);
        Ok((rotate(inverse, [p[0] - rp[0], p[1] - rp[1], p[2] - rp[2]]), mul(inverse, r), s))
    }

    fn has_script(&mut self, node: Id, class: &str) -> Result<bool, String> {
        let components = self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default();
        for (c, k) in components {
            if k == unity::MONO_BEHAVIOUR && self.graph.script(&mut self.data, c)?.is_some_and(|s| s.1 == class) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// GameData.PanelsOf: the active Panels below the root, each with its type, pose and the
    /// enabled box colliders on its own object.
    fn panels_of(&mut self, root: Id) -> Result<Value, String> {
        let mut out = Vec::new();
        for (c, node, _, class) in self.graph.behaviours_in_children(&mut self.data, root)? {
            if class != "Panel" || !self.graph.active_in_hierarchy(node) {
                continue;
            }
            let b = self.behaviour(c)?;
            let panel_type = match field(&b.fields, "panelType") {
                Some(Raw::Enum(v, names, flags)) => super::serial::enum_name(*v, names, *flags),
                other => return Err(format!("Panel.panelType: {other:?}")),
            };
            let (p, r, _) = self.in_root(root, node)?;
            let mut boxes = Vec::new();
            for (k, class_id) in self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default() {
                if class_id != unity::BOX_COLLIDER {
                    continue;
                }
                let collider = self.data.reader(k).and_then(|mut rd| unity::read_collider(class_id, &mut rd)).ok_or("unreadable BoxCollider")?;
                if let (true, unity::Shape::Box { center, size }) = (collider.enabled, collider.shape) {
                    boxes.push(json!([v3(center), v3(size)]));
                }
            }
            out.push(json!({"panelType": panel_type, "position": v3(p), "rotation": json!(r.map(single).to_vec()), "boxes": boxes}));
        }
        Ok(Value::Array(out))
    }

    /// BoundsHelper.GetBoundsOfChildren of a holder at the origin with `look` in it: from the
    /// origin, every active Renderer's world bounds (the mesh's local bounds through its
    /// transform), skipping those `skip` names a component of.
    fn look_bounds(&mut self, look: Id, skip: Option<&str>) -> Result<Value, String> {
        self.graph_file(look.0)?;
        let (mut min, mut max) = ([0f32; 3], [0f32; 3]);
        for node in self.graph.descendants(look) {
            if !self.graph.active_in_hierarchy(node) {
                continue;
            }
            let components = self.graph.nodes.get(&node).map(|n| n.components.clone()).unwrap_or_default();
            for (_, class_id) in &components {
                let name = unity::class_name(*class_id).ok_or_else(|| format!("unknown Unity class id {class_id} in a panel look (add it to native/unity.rs class_name)"))?;
                if !name.ends_with("Renderer") {
                    continue;
                }
                if let Some(s) = skip && self.has_script(node, s)? {
                    continue;
                }
                if *class_id == PARTICLE_SYSTEM_RENDERER {
                    // A particle system just instantiated has no particles: its bounds are the
                    // point it stands at.
                    let (p, _, _) = self.in_root(look, node)?;
                    for i in 0..3 {
                        min[i] = min[i].min(p[i]);
                        max[i] = max[i].max(p[i]);
                    }
                    continue;
                }
                if *class_id != MESH_RENDERER {
                    return Err(format!("{name} in a panel look: only mesh and particle renderers are read"));
                }
                let filter = components.iter().find(|(_, k)| *k == unity::MESH_FILTER).map(|(f, _)| *f).ok_or("a MeshRenderer without a MeshFilter")?;
                let mesh = self.data.reader(filter).and_then(|mut r| {
                    r.pptr()?;
                    r.pptr()
                });
                let mesh = mesh.ok_or("an unreadable MeshFilter")?;
                let mesh = self.data.resolve_builtin(filter.0, mesh)?.ok_or("a MeshFilter without a mesh")?;
                let file = &self.data.files[mesh.0];
                let data = file.data(file.object(mesh.1).ok_or("no mesh object")?);
                let dir = self.data.dir.clone();
                // A stream path is "<file>.resS" next to the data, or "archive:/<cab>/<file>".
                let stream = |path: &str, offset: u64, size: usize| -> Result<Vec<u8>, String> {
                    use std::io::{Read, Seek, SeekFrom};
                    let name = path.rsplit('/').next().unwrap_or(path);
                    let file = dir.join(name);
                    let fail = |e: &dyn std::fmt::Display| format!("vertex stream {}: {e}", file.display());
                    let mut f = std::fs::File::open(&file).map_err(|e| fail(&e))?;
                    let len = f.metadata().map_err(|e| fail(&e))?.len();
                    if offset.checked_add(size as u64).is_none_or(|end| end > len) {
                        return Err(fail(&format!("{size} bytes at {offset} lie past its end ({len} bytes)")));
                    }
                    f.seek(SeekFrom::Start(offset)).map_err(|e| fail(&e))?;
                    let mut buf = vec![0; size];
                    f.read_exact(&mut buf).map_err(|e| fail(&e))?;
                    Ok(buf)
                };
                let (center, extent) = super::typetree::mesh_bounds(&file.unity_version, data, &stream)?;
                let (p, r, s) = self.in_root(look, node)?;
                // Renderer.bounds: the local box through the transform, as an axis-aligned box.
                let c = rotate(r, [center[0] * s[0], center[1] * s[1], center[2] * s[2]]);
                let columns = [0, 1, 2].map(|j| {
                    let mut unit = [0f32; 3];
                    unit[j] = s[j];
                    rotate(r, unit)
                });
                for i in 0..3 {
                    let half: f32 = (0..3).map(|j| columns[j][i].abs() * extent[j]).sum();
                    min[i] = min[i].min(p[i] + c[i] - half);
                    max[i] = max[i].max(p[i] + c[i] + half);
                }
            }
        }
        Ok(json!([v3(min), v3(max)]))
    }

    fn look_entry(&mut self, look: Id) -> Result<Map<String, Value>, String> {
        let mut entry = Map::new();
        entry.insert("bounds".into(), self.look_bounds(look, None)?);
        entry.insert("panelBounds".into(), self.look_bounds(look, Some("IgnoreForPanelBounds"))?);
        Ok(entry)
    }

    /// GameData panels: {"groups": building id -> its panels, "looks": sub type -> bounds}.
    pub fn panels(&mut self, resources: Id) -> Result<Value, String> {
        let mut groups = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let Some(root) = prefab else { continue };
            self.graph_file(root.0)?;
            if self.network_object_of(root)?.is_none() {
                continue;
            }
            let has_panel = self.graph.behaviours_in_children(&mut self.data, root)?.into_iter().any(|(_, node, _, class)| class == "Panel" && self.graph.active_in_hierarchy(node));
            if has_panel {
                groups.insert(gid.clone(), self.panels_of(root).map_err(|e| format!("{gid}: {e}"))?);
            }
        }
        // PanelsResources.GetPanelGameObject / GetPanelUndergroundGameObject: the first entry
        // of the sub type; the underground look when it has one.
        let b = self.behaviour(resources)?;
        let list = |n: &str| match field(&b.fields, n) {
            Some(Raw::List(items)) => Ok(items.clone()),
            other => Err(format!("PanelsResources.{n}: {other:?}")),
        };
        let (types, looks, underground) = (list("panelsSubtypes")?, list("panelsGameObjects")?, list("panelsUndergroundGameObjects")?);
        let sub_type = super::dotnet::TypeName { scope: Some("Assembly-CSharp".into()), namespace: "SpaceCraft".into(), name: "DataConfig/BuildPanelSubType".into() };
        let mut values: Vec<(i64, String)> = self.layouts.asm.def(&sub_type).ok_or("no DataConfig.BuildPanelSubType")?.fields.iter().filter_map(|f| Some((f.constant?, f.name.clone()))).collect();
        values.sort();
        let mut out = Map::new();
        for (value, name) in values {
            let Some(i) = types.iter().position(|t| matches!(t, Raw::Enum(v, ..) if *v == value)) else { continue };
            let resolve = |ctx: &mut Ctx, list: &str, r: Option<&Raw>| match r {
                Some(Raw::Ref(p, _)) => ctx.data.resolve(resources.0, *p),
                other => Err(format!("PanelsResources.{list}[{i}]: {other:?}")),
            };
            let Some(look) = resolve(self, "panelsGameObjects", looks.get(i))? else { continue };
            let mut entry = self.look_entry(look).map_err(|e| format!("{name}: {e}"))?;
            if let Some(under) = resolve(self, "panelsUndergroundGameObjects", underground.get(i))?.filter(|u| *u != look) {
                entry.insert("underground".into(), Value::Object(self.look_entry(under).map_err(|e| format!("{name} underground: {e}"))?));
            }
            out.insert(name, Value::Object(entry));
        }
        Ok(json!({"groups": groups, "looks": out}))
    }
}
