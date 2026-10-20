//! Optional collider map, read separately so the regular tables and snapshot formats stay valid.
use super::{
    tables::Ctx,
    unity::{self, Id},
};
use crate::geometry::{
    self,
    navigation::{Bounds, Data},
};
use std::collections::HashMap;
use std::path::Path;

impl Ctx {
    fn navigation_bounds(&mut self, nodes: Vec<Id>, root: Option<Id>, meshes: &mut HashMap<Id, Bounds>) -> Result<Vec<Bounds>, String> {
        let mut boxes = Vec::new();
        for node in nodes {
            let components = self.graph.nodes[&node].components.clone();
            for (id, class) in components {
                if class == 154 {
                    return Err("TerrainCollider is not supported by the drone map reader".into());
                }
                if ![unity::BOX_COLLIDER, unity::SPHERE_COLLIDER, unity::CAPSULE_COLLIDER, unity::MESH_COLLIDER].contains(&class) {
                    continue;
                }
                let mut reader = self.data.reader(id).ok_or("missing collider")?;
                let collider = unity::read_collider(class, &mut reader).ok_or("unreadable collider")?;
                if collider.trigger || !collider.enabled {
                    continue;
                }
                let (center, half) = match collider.shape {
                    unity::Shape::Box { center, size } => (center, size.map(|v| v * 0.5)),
                    unity::Shape::Sphere { center, radius } => (center, [radius; 3]),
                    unity::Shape::Capsule {
                        center,
                        radius,
                        height,
                        direction,
                    } => {
                        let mut half = [radius; 3];
                        if !(0..3).contains(&direction) {
                            return Err("invalid capsule direction".into());
                        }
                        half[direction as usize] = radius.max(height * 0.5);
                        (center, half)
                    }
                    unity::Shape::Mesh => {
                        // Unity 6000.3 MeshCollider: convex, alignment, cooking options, mesh.
                        reader.bool().ok_or("MeshCollider.convex")?;
                        reader.align();
                        reader.i32().ok_or("MeshCollider.cookingOptions")?;
                        let pointer = reader.pptr().ok_or("MeshCollider.mesh")?;
                        let Some(mesh) = self.data.resolve_builtin(id.0, pointer)? else {
                            continue;
                        };
                        if let std::collections::hash_map::Entry::Vacant(e) = meshes.entry(mesh) {
                            let file = &self.data.files[mesh.0];
                            let bytes = file.data(file.object(mesh.1).ok_or("missing mesh")?);
                            let dir = self.data.dir.clone();
                            let stream = |path: &str, offset: u64, size: usize| -> Result<Vec<u8>, String> {
                                use std::io::{Read, Seek, SeekFrom};
                                let mut f = std::fs::File::open(dir.join(path.rsplit('/').next().unwrap_or(path))).map_err(|e| e.to_string())?;
                                if offset.checked_add(size as u64).is_none_or(|end| f.metadata().map(|m| end > m.len()).unwrap_or(true)) {
                                    return Err("mesh stream past end".into());
                                }
                                f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                                let mut out = vec![0; size];
                                f.read_exact(&mut out).map_err(|e| e.to_string())?;
                                Ok(out)
                            };
                            let (c, h) = super::typetree::mesh_bounds(&file.unity_version, bytes, &stream)?;
                            e.insert(Bounds {
                                min: [0, 1, 2].map(|i| c[i] - h[i]),
                                max: [0, 1, 2].map(|i| c[i] + h[i]),
                            });
                        }
                        let b = &meshes[&mesh];
                        ([0, 1, 2].map(|i| (b.min[i] + b.max[i]) * 0.5), [0, 1, 2].map(|i| (b.max[i] - b.min[i]) * 0.5))
                    }
                };
                let (p, r, scale) = if let Some(root) = root { self.in_root(root, node)? } else { self.graph.world(node)? };
                let c = geometry::rotate(r, [0, 1, 2].map(|i| center[i] * scale[i]));
                let axes = [0, 1, 2].map(|i| {
                    let mut v = [0.0; 3];
                    v[i] = half[i] * scale[i];
                    geometry::rotate(r, v)
                });
                let h = [0, 1, 2].map(|i| axes.iter().map(|a| a[i].abs()).sum::<f32>());
                boxes.push(Bounds {
                    min: [0, 1, 2].map(|i| p[i] + c[i] - h[i]),
                    max: [0, 1, 2].map(|i| p[i] + c[i] + h[i]),
                });
            }
        }
        Ok(boxes)
    }
}

pub fn extract(game: &Path, tables: &Path) -> Result<Data, String> {
    let build = crate::tables::build_id(game)?;
    let expected = std::fs::read_to_string(tables.join("buildid")).map_err(|e| format!("tables buildid: {e}"))?;
    if expected.trim() != build {
        return Err("game and tables builds differ; extract matching tables first".into());
    }
    let mut ctx = Ctx::open(game).map_err(|e| e.to_string())?;
    let assets = ctx.assets()?;
    let planets = ctx.planets(&assets)?;
    let mut out = Data {
        format: 1,
        build_id: build,
        ..Default::default()
    };
    let mut meshes = HashMap::new();
    for (id, root) in ctx.groups.clone() {
        let Some(root) = root else {
            continue;
        };
        let constructible = assets.iter().any(|(k, v)| k.starts_with("GroupDataConstructible:") && v["id"].as_str() == Some(&id));
        if !constructible {
            continue;
        }
        ctx.graph_file(root.0)?;
        let nodes = ctx.graph.descendants(root);
        let bounds = ctx.navigation_bounds(nodes, Some(root), &mut meshes).map_err(|e| format!("{id}: {e}"))?;
        if !bounds.is_empty() {
            out.buildings.insert(id, bounds);
        }
    }
    for planet in planets {
        let id = planet["id"].as_str().ok_or("planet id")?;
        let data = assets.get(&format!("PlanetData:PlanetData-{id}")).ok_or("planet data")?;
        let files = ctx.planet_files(data["sceneName"].as_str().ok_or("planet scene")?)?;
        let mut nodes: Vec<_> = ctx.graph.nodes.keys().copied().filter(|n| files.contains(&n.0)).collect();
        nodes.sort_by_key(|n| (n.0, n.1));
        // Include inactive scene colliders conservatively: terraformation may reveal them later.
        let bounds = ctx.navigation_bounds(nodes, None, &mut meshes).map_err(|e| format!("{id}: {e}"))?;
        if bounds.is_empty() { return Err(format!("{id}: no terrain colliders found")); }
        eprintln!("drone-map: {id}: {} collider bounds", bounds.len());
        out.planets.insert(crate::data::stable_hash(id), bounds);
    }
    out.validate(&expected)?;
    Ok(out)
}

pub fn run(args: &[String]) {
    let result = (|| -> Result<(), String> {
        if args.len() != 2 {
            return Err("usage: pcds drone-map GAME_DIR TABLES_DIR".into());
        }
        let tables = Path::new(&args[1]);
        let map = extract(Path::new(&args[0]), tables)?;
        let tmp = tables.join("drone-navigation.json.tmp");
        let text = serde_json::to_vec(&map).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(tmp, tables.join("drone-navigation.json")).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("drone-map: {e}");
        std::process::exit(1);
    }
}
