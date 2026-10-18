//! The object graph of loaded files: game objects with their components and transform
//! hierarchy, as Unity walks them (GetComponentsInChildren: the object's own components, then
//! each child's, depth first), with world transforms composed from the local ones.

use std::collections::HashMap;

use super::unity::{self, Data, Id};

pub struct Node {
    pub name: String,
    pub active: bool,
    pub layer: u32,
    /// Components in the game object's order, the Transform first.
    pub components: Vec<(Id, i32)>,
    pub transform: Option<Id>,
}

pub struct Xform {
    pub game_object: Option<Id>,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    pub children: Vec<Id>,
    pub father: Option<Id>,
}

/// Game objects and transforms read so far, with each MonoBehaviour's script class.
#[derive(Default)]
pub struct Graph {
    pub nodes: HashMap<Id, Node>,
    pub xforms: HashMap<Id, Xform>,
    /// MonoScript -> (namespace, class, assembly).
    scripts: HashMap<Id, (String, String, String)>,
    /// MonoBehaviour -> its script.
    behaviour_scripts: HashMap<Id, Option<Id>>,
}

impl Graph {
    /// Reads every game object and transform of a file.
    pub fn add_file(&mut self, data: &mut Data, file: usize) {
        let objects: Vec<(i64, i32)> = data.files[file].objects.iter().map(|o| (o.path, o.class_id)).collect();
        for (path, class) in objects {
            let id = Id(file, path);
            match class {
                unity::GAME_OBJECT => {
                    let Some(g) = data.reader(id).and_then(|mut r| unity::game_object(&mut r)) else { continue };
                    let mut components: Vec<(Id, i32)> = Vec::new();
                    for p in &g.components {
                        if let Some(c) = data.resolve(file, *p) {
                            components.push((c, data.class_of(c).unwrap_or(0)));
                        }
                    }
                    let transform = components.iter().find(|(_, c)| *c == unity::TRANSFORM || *c == unity::RECT_TRANSFORM).map(|(c, _)| *c);
                    self.nodes.insert(id, Node { name: g.name, active: g.active, layer: g.layer, components, transform });
                }
                unity::TRANSFORM | unity::RECT_TRANSFORM => {
                    let Some(t) = data.reader(id).and_then(|mut r| unity::transform(&mut r)) else { continue };
                    let game_object = data.resolve(file, t.game_object);
                    let mut children = Vec::new();
                    for c in &t.children {
                        children.extend(data.resolve(file, *c));
                    }
                    let father = data.resolve(file, t.father);
                    self.xforms.insert(id, Xform { game_object, position: t.position, rotation: t.rotation, scale: t.scale, children, father });
                }
                _ => {}
            }
        }
    }

    /// The script class of a MonoBehaviour: (namespace, class, assembly).
    pub fn script(&mut self, data: &mut Data, behaviour: Id) -> Option<&(String, String, String)> {
        let script = match self.behaviour_scripts.get(&behaviour) {
            Some(s) => *s,
            None => {
                let s = data.reader(behaviour).and_then(|mut r| unity::behaviour(&mut r)).and_then(|b| data.resolve(behaviour.0, b.script));
                self.behaviour_scripts.insert(behaviour, s);
                s
            }
        }?;
        if !self.scripts.contains_key(&script) {
            let s = data.reader(script).and_then(|mut r| unity::mono_script(&mut r))?;
            self.scripts.insert(script, (s.namespace, s.class, s.assembly));
        }
        self.scripts.get(&script)
    }

    pub fn node_of_transform(&self, t: Id) -> Option<Id> {
        self.xforms.get(&t)?.game_object
    }

    /// Transform children of a game object, in order.
    pub fn children(&self, node: Id) -> Vec<Id> {
        let Some(t) = self.nodes.get(&node).and_then(|n| n.transform) else { return Vec::new() };
        self.xforms.get(&t).map(|x| x.children.iter().filter_map(|c| self.node_of_transform(*c)).collect()).unwrap_or_default()
    }

    pub fn parent(&self, node: Id) -> Option<Id> {
        let t = self.nodes.get(&node)?.transform?;
        self.node_of_transform(self.xforms.get(&t)?.father?)
    }

    /// The object and everything under it, depth first (GetComponentsInChildren order).
    pub fn descendants(&self, node: Id) -> Vec<Id> {
        let mut out = vec![node];
        for c in self.children(node) {
            out.extend(self.descendants(c));
        }
        out
    }

    /// activeInHierarchy: the object and all its parents active.
    pub fn active_in_hierarchy(&self, node: Id) -> bool {
        let mut n = Some(node);
        while let Some(id) = n {
            if !self.nodes.get(&id).is_some_and(|x| x.active) {
                return false;
            }
            n = self.parent(id);
        }
        true
    }

    /// The path below `root` ("" for the root itself), as GameData.RelativePath writes it.
    pub fn path(&self, root: Id, node: Id) -> String {
        if node == root {
            return String::new();
        }
        let name = self.nodes.get(&node).map(|n| n.name.clone()).unwrap_or_default();
        match self.parent(node) {
            Some(p) if p != root => format!("{}/{name}", self.path(root, p)),
            _ => name,
        }
    }

    /// World position, rotation and lossy scale of a game object (its transform's local values
    /// composed up the hierarchy).
    pub fn world(&self, node: Id) -> ([f32; 3], [f32; 4], [f32; 3]) {
        let Some(t) = self.nodes.get(&node).and_then(|n| n.transform).and_then(|t| self.xforms.get(&t)) else { return ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]) };
        match t.father.and_then(|f| self.node_of_transform(f)) {
            None => (t.position, t.rotation, t.scale),
            Some(parent) => {
                let (pp, pr, ps) = self.world(parent);
                let scaled = [t.position[0] * ps[0], t.position[1] * ps[1], t.position[2] * ps[2]];
                let p = crate::geometry::rotate(pr, scaled);
                (
                    [pp[0] + p[0], pp[1] + p[1], pp[2] + p[2]],
                    crate::geometry::mul(pr, t.rotation),
                    [ps[0] * t.scale[0], ps[1] * t.scale[1], ps[2] * t.scale[2]],
                )
            }
        }
    }

    /// MonoBehaviours on the object and under it with their script class, in Unity's order.
    pub fn behaviours_in_children(&mut self, data: &mut Data, node: Id) -> Vec<(Id, Id, String, String)> {
        let mut out = Vec::new();
        for n in self.descendants(node) {
            let components: Vec<(Id, i32)> = self.nodes.get(&n).map(|x| x.components.clone()).unwrap_or_default();
            for (c, class) in components {
                if class == unity::MONO_BEHAVIOUR {
                    if let Some((ns, cl, _)) = self.script(data, c).cloned() {
                        out.push((c, n, ns, cl));
                    }
                }
            }
        }
        out
    }

    /// Roots of a file's hierarchy (scene roots, prefab roots): transforms without a father.
    pub fn roots(&self, file: usize) -> Vec<Id> {
        let mut roots: Vec<Id> = self.xforms.iter().filter(|(id, x)| id.0 == file && x.father.is_none()).filter_map(|(_, x)| x.game_object).collect();
        roots.sort();
        roots
    }
}
