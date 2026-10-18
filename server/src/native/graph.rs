//! The object graph of loaded files: game objects with their components and transform
//! hierarchy, as Unity walks them (GetComponentsInChildren: the object's own components, then
//! each child's, depth first), with world transforms composed from the local ones.

use std::collections::HashMap;

use super::unity::{self, Data, Id};

pub struct Node {
    pub name: String,
    pub active: bool,
    /// Components in the game object's order, the Transform first.
    pub components: Vec<(Id, i32)>,
    pub transform: Id,
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
    /// Reads every game object and transform of a file: each object must be readable, its
    /// components and its transform's hierarchy found, all in the file.
    pub fn add_file(&mut self, data: &mut Data, file: usize) -> Result<(), String> {
        let name = data.files[file].name.clone();
        let objects: Vec<(i64, i32)> = data.files[file].objects.iter().map(|o| (o.path, o.class_id)).collect();
        let (mut nodes, mut xforms): (HashMap<Id, Node>, HashMap<Id, Xform>) = Default::default();
        for (path, class) in objects {
            let id = Id(file, path);
            match class {
                unity::GAME_OBJECT => {
                    let g = data.reader(id).and_then(|mut r| unity::game_object(&mut r)).ok_or_else(|| format!("{name}: unreadable GameObject {path}"))?;
                    let mut components: Vec<(Id, i32)> = Vec::new();
                    for p in &g.components {
                        let c = data.resolve(file, *p)?.ok_or_else(|| format!("{name}: GameObject {} has a null or built-in component", g.name))?;
                        components.push((c, data.class_of(c).expect("resolve finds the object")));
                    }
                    let transform = components.iter().find(|(_, c)| *c == unity::TRANSFORM || *c == unity::RECT_TRANSFORM).map(|(c, _)| *c).ok_or_else(|| format!("{name}: GameObject {} without a Transform", g.name))?;
                    nodes.insert(id, Node { name: g.name, active: g.active, components, transform });
                }
                unity::TRANSFORM | unity::RECT_TRANSFORM => {
                    let t = data.reader(id).and_then(|mut r| unity::transform(&mut r)).ok_or_else(|| format!("{name}: unreadable Transform {path}"))?;
                    let game_object = data.resolve(file, t.game_object)?;
                    let mut children = Vec::new();
                    for c in &t.children {
                        children.push(data.resolve(file, *c)?.ok_or_else(|| format!("{name}: Transform {path} has a null child"))?);
                    }
                    let father = data.resolve(file, t.father)?;
                    xforms.insert(id, Xform { game_object, position: t.position, rotation: t.rotation, scale: t.scale, children, father });
                }
                _ => {}
            }
        }
        // Each hierarchy lies in its file: what the walks below follow is in the graph.
        for (id, n) in &nodes {
            if xforms.get(&n.transform).is_none_or(|x| x.game_object != Some(*id)) {
                return Err(format!("{name}: GameObject {}'s Transform is not its own", n.name));
            }
        }
        for (id, x) in &xforms {
            let linked = x.game_object.is_some_and(|g| nodes.contains_key(&g)) && x.father.iter().chain(&x.children).all(|t| xforms.contains_key(t));
            if !linked {
                return Err(format!("{name}: Transform {} links to objects outside the file", id.1));
            }
        }
        self.nodes.extend(nodes);
        self.xforms.extend(xforms);
        Ok(())
    }

    /// The script class of a MonoBehaviour: (namespace, class, assembly); None for a null
    /// script (a missing one).
    pub fn script(&mut self, data: &mut Data, behaviour: Id) -> Result<Option<&(String, String, String)>, String> {
        let script = match self.behaviour_scripts.get(&behaviour) {
            Some(s) => *s,
            None => {
                let b = data.reader(behaviour).and_then(|mut r| unity::behaviour(&mut r)).ok_or_else(|| format!("{}: unreadable MonoBehaviour {}", data.files[behaviour.0].name, behaviour.1))?;
                let s = data.resolve(behaviour.0, b.script)?;
                self.behaviour_scripts.insert(behaviour, s);
                s
            }
        };
        let Some(script) = script else { return Ok(None) };
        if !self.scripts.contains_key(&script) {
            let s = data.reader(script).and_then(|mut r| unity::mono_script(&mut r)).ok_or_else(|| format!("{}: unreadable MonoScript {}", data.files[script.0].name, script.1))?;
            self.scripts.insert(script, (s.namespace, s.class, s.assembly));
        }
        Ok(self.scripts.get(&script))
    }

    pub fn node_of_transform(&self, t: Id) -> Option<Id> {
        self.xforms.get(&t)?.game_object
    }

    /// A game object's transform (add_file checked every node has one in the graph).
    pub fn xform(&self, node: Id) -> Option<&Xform> {
        self.xforms.get(&self.nodes.get(&node)?.transform)
    }

    /// Transform children of a game object, in order.
    pub fn children(&self, node: Id) -> Vec<Id> {
        self.xform(node).map(|x| x.children.iter().filter_map(|c| self.node_of_transform(*c)).collect()).unwrap_or_default()
    }

    pub fn parent(&self, node: Id) -> Option<Id> {
        self.node_of_transform(self.xform(node)?.father?)
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
    /// composed up the hierarchy); an error for an object not in the graph.
    pub fn world(&self, node: Id) -> Result<([f32; 3], [f32; 4], [f32; 3]), String> {
        let t = self.xform(node).ok_or_else(|| format!("{node:?}: not a game object of a file read"))?;
        Ok(match t.father.and_then(|f| self.node_of_transform(f)) {
            None => (t.position, t.rotation, t.scale),
            Some(parent) => {
                let (pp, pr, ps) = self.world(parent)?;
                let scaled = [t.position[0] * ps[0], t.position[1] * ps[1], t.position[2] * ps[2]];
                let p = crate::geometry::rotate(pr, scaled);
                (
                    [pp[0] + p[0], pp[1] + p[1], pp[2] + p[2]],
                    crate::geometry::mul(pr, t.rotation),
                    [ps[0] * t.scale[0], ps[1] * t.scale[1], ps[2] * t.scale[2]],
                )
            }
        })
    }

    /// MonoBehaviours on the object and under it with their script class, in Unity's order.
    pub fn behaviours_in_children(&mut self, data: &mut Data, node: Id) -> Result<Vec<(Id, Id, String, String)>, String> {
        let mut out = Vec::new();
        for n in self.descendants(node) {
            let components: Vec<(Id, i32)> = self.nodes.get(&n).map(|x| x.components.clone()).unwrap_or_default();
            for (c, class) in components {
                if class == unity::MONO_BEHAVIOUR {
                    if let Some((ns, cl, _)) = self.script(data, c)?.cloned() {
                        out.push((c, n, ns, cl));
                    }
                }
            }
        }
        Ok(out)
    }

}
