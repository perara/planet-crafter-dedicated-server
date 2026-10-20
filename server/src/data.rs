//! Static per-game-version tables (data/<version>/netdata.json, read from the game's files by
//! `pcds read-game`) and the game's string hash.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde::Deserialize;

use crate::compact::{Bytes, Sym};

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NetData {
    pub game_version: String,
    pub tick_rate: u32,
    /// (type-name hash, version) of every Netcode message, in the server's id order.
    pub message_versions: Vec<(u32, i32)>,
    pub scene_objects: Vec<ObjectDef>,
    pub prefabs: HashMap<String, ObjectDef>,
    pub planets: Vec<PlanetDef>,
}

/// A network object as a spawn describes it (an in-scene object, a prefab, a world object's
/// layout).
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDef {
    /// In-scene objects: their network object id.
    #[serde(default)]
    pub id: u64,
    /// World object layouts: the prefab's name.
    #[serde(default)]
    pub prefab: String,
    pub hash: u32,
    pub flags: u16,
    pub parent: Option<u64>,
    pub behaviours: Vec<BehaviourDef>,
    /// Its NetworkTransforms' spawn states (NetworkTransform.OnSynchronize).
    pub transforms: Vec<TransformDef>,
    pub scale: Option<[f32; 3]>,
    /// Whether a world object of this prefab keeps its group hash at spawn (the root carries
    /// a GroupNetworkBase and has children).
    pub group_hash_on_spawn: bool,
    /// `behaviours` made ready to spawn from, the first time one is spawned.
    #[serde(skip)]
    compiled: OnceLock<Arc<Compiled>>,
}

impl ObjectDef {
    /// The behaviours' shape and starting values, made once.
    pub fn compiled(&self) -> Arc<Compiled> {
        self.compiled.get_or_init(|| Arc::new(Compiled::of(&self.behaviours))).clone()
    }
}

/// What the network objects made from one layout (or one signature) have in common: the classes
/// and indices of their behaviours and the names and types of their variables, in order. A
/// network object holds only its values and points at one of these (there are a few hundred in
/// the whole game: 1.7 KB an object of names and types became 8 bytes).
#[derive(Debug)]
pub struct Shape {
    pub behaviours: Box<[ShapeBehaviour]>,
    pub vars: Box<[ShapeVar]>,
}

#[derive(Clone, Copy, Debug)]
pub struct ShapeBehaviour {
    pub class: Sym,
    pub index: u16,
    /// Its variables are `vars[first..first + count]`.
    pub first: u16,
    pub count: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct ShapeVar {
    pub name: Sym,
    pub ty: Sym,
    /// A NetworkList<T>.
    pub list: bool,
    /// A NetworkVariable<FixedString..>: its delta starts with a flag byte.
    pub fixed_string: bool,
}

/// The shapes made so far, by the hash of their signature (see `Signature`), with the signatures
/// to tell apart the rare two that hash alike.
type Shapes = HashMap<u64, Vec<(Box<[usize]>, &'static Shape)>>;

fn shapes() -> &'static RwLock<Shapes> {
    static SHAPES: OnceLock<RwLock<Shapes>> = OnceLock::new();
    SHAPES.get_or_init(Default::default)
}

/// A shape written as numbers: per behaviour its class, index and number of variables, then per
/// variable its name and type (the addresses of the words). Two shapes are the same when these are.
#[derive(Default)]
pub struct Signature(Vec<usize>);

impl Signature {
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn behaviour(&mut self, class: Sym, index: u16, vars: usize) {
        self.0.extend([class.addr(), index as usize, vars]);
    }
    pub fn var(&mut self, name: Sym, ty: Sym) {
        self.0.extend([name.addr(), ty.addr()]);
    }
}

impl Shape {
    /// The shape with this signature: the one made before, or the one `build` makes now.
    pub fn intern(sig: &Signature, build: impl FnOnce() -> Shape) -> &'static Shape {
        let hash = sig.0.iter().fold(0u64, |h, w| (h.rotate_left(5) ^ *w as u64).wrapping_mul(0x517c_c1b7_2722_0a95));
        let known = |t: &Shapes| t.get(&hash).and_then(|same| same.iter().find(|(s, _)| s[..] == sig.0[..])).map(|(_, shape)| *shape);
        let table = shapes();
        if let Some(shape) = known(&table.read().unwrap_or_else(|e| e.into_inner())) {
            return shape;
        }
        let mut write = table.write().unwrap_or_else(|e| e.into_inner());
        if let Some(shape) = known(&write) {
            return shape;
        }
        let shape: &'static Shape = Box::leak(Box::new(build()));
        write.entry(hash).or_default().push((sig.0.clone().into_boxed_slice(), shape));
        shape
    }

    /// A shape from its behaviours' parts: (class, index, [(name, type)]).
    pub fn build<'a>(behaviours: impl Iterator<Item = (Sym, u16, &'a [(Sym, Sym)])>) -> Shape {
        let (mut shape_behaviours, mut vars) = (Vec::new(), Vec::new());
        for (class, index, list) in behaviours {
            let first = u16::try_from(vars.len()).expect("fewer than 65536 variables on an object");
            let count = u16::try_from(list.len()).expect("fewer than 65536 variables on a behaviour");
            shape_behaviours.push(ShapeBehaviour { class, index, first, count });
            vars.extend(list.iter().map(|&(name, ty)| ShapeVar { name, ty, list: ty.starts_with("NetworkList"), fixed_string: ty.starts_with("NetworkVariable<FixedString") }));
        }
        Shape { behaviours: shape_behaviours.into(), vars: vars.into() }
    }
}

/// A layout's behaviours ready to spawn from: their shape, and the value each variable starts
/// with (empty: the server sets it).
#[derive(Debug)]
pub struct Compiled {
    pub shape: &'static Shape,
    pub defaults: Box<[Bytes]>,
}

impl Compiled {
    pub fn of(defs: &[BehaviourDef]) -> Compiled {
        let mut sig = Signature::default();
        for b in defs {
            sig.behaviour(b.class, b.behaviour_id, b.vars.len());
            b.vars.iter().for_each(|v| sig.var(v.name, v.ty));
        }
        let shape = Shape::intern(&sig, || {
            let pairs: Vec<Vec<(Sym, Sym)>> = defs.iter().map(|b| b.vars.iter().map(|v| (v.name, v.ty)).collect()).collect();
            Shape::build(defs.iter().zip(&pairs).map(|(b, p)| (b.class, b.behaviour_id, p.as_slice())))
        });
        let defaults = defs.iter().flat_map(|b| &b.vars).map(|v| v.bytes.as_deref().map(|h| Bytes::from(crate::world::hex(h))).unwrap_or_default()).collect();
        Compiled { shape, defaults }
    }
}

#[derive(Deserialize, Clone)]
pub struct TransformDef {
    /// The NetworkBehaviour index.
    pub behaviour: u16,
    /// NetworkTransformState flags of a synchronization.
    pub flags: u32,
    /// On the object's root: its position is the object's.
    pub root: bool,
    /// The synchronized axes' values: positions, then angles.
    pub values: Vec<f32>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BehaviourDef {
    pub class: crate::compact::Sym,
    pub behaviour_id: u16,
    pub vars: Vec<VarDef>,
}

#[derive(Deserialize, Clone)]
pub struct VarDef {
    pub name: crate::compact::Sym,
    /// Netcode type, e.g. "NetworkVariable<Int32>".
    #[serde(rename = "type", default)]
    pub ty: crate::compact::Sym,
    /// The value it starts with (hex), as the game's constructor gives it; None when the server
    /// must set it (types with their own serializer).
    pub bytes: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanetDef {
    pub id: String,
    pub steam_app_id: u32,
    /// PlanetData.startingPlanet: a new world can start here.
    #[serde(default)]
    pub starting_planet: bool,
}

impl NetData {
    pub fn load(path: &str) -> Result<NetData, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    pub fn planet_index(&self, id: &str) -> Option<usize> {
        self.planets.iter().position(|p| p.id == id)
    }
}

/// StringExtensionMethods.GetStableHashCode: the .NET Framework string hash over UTF-16 units.
pub fn stable_hash(s: &str) -> i32 {
    let mut units = s.encode_utf16();
    let mut h1: i32 = 5381;
    let mut h2: i32 = h1;
    // Pairs of units up to a NUL, as the game reads the string (no Vec: this runs on hot paths).
    while let Some(a) = units.next().filter(|u| *u != 0) {
        h1 = (h1.wrapping_shl(5).wrapping_add(h1)) ^ a as i32;
        let Some(b) = units.next().filter(|u| *u != 0) else { break };
        h2 = (h2.wrapping_shl(5).wrapping_add(h2)) ^ b as i32;
    }
    h1.wrapping_add(h2.wrapping_mul(1566083941))
}

#[cfg(test)]
mod tests {
    use super::stable_hash;

    #[test]
    fn stable_hash_matches_the_game() {
        assert_eq!(stable_hash("Prime"), -1140328421);
        assert_eq!(stable_hash("Humble"), -486276833);
        assert_eq!(stable_hash("Container1"), 1768717124);
        assert_eq!(stable_hash("CraftStation1"), 769498401);
        assert_eq!(stable_hash("QuartzCraftStation1"), -1824094490);
    }

    /// The walk over the UTF-16 units is the one over a collected Vec (what it replaced): odd
    /// and even lengths, a NUL ends the string, units outside the BMP come as two.
    #[test]
    fn stable_hash_without_a_vec_is_the_same_hash() {
        fn reference(s: &str) -> i32 {
            let units: Vec<u16> = s.encode_utf16().collect();
            let (mut h1, mut h2): (i32, i32) = (5381, 5381);
            let mut i = 0;
            while i < units.len() && units[i] != 0 {
                h1 = (h1.wrapping_shl(5).wrapping_add(h1)) ^ units[i] as i32;
                if i == units.len() - 1 || units[i + 1] == 0 {
                    break;
                }
                h2 = (h2.wrapping_shl(5).wrapping_add(h2)) ^ units[i + 1] as i32;
                i += 2;
            }
            h1.wrapping_add(h2.wrapping_mul(1566083941))
        }
        for s in ["", "a", "ab", "abc", "abcd", "\0", "a\0b", "ab\0c", "abc\0d", "Prime", "\u{e5}\u{f8}z", "x\u{1F600}y", "\u{1F600}"] {
            assert_eq!(stable_hash(s), reference(s), "{s:?}");
        }
    }
}
