//! Tessera (with the DeBroglie solver and the Sylves cube grid it runs on) as The Planet
//! Crafter's wreck generators use it: TesseraGenerator.Generate on a cube grid with an
//! AdjacentPaint model, the Default algorithm (AC4), the Default index and tile pickers
//! (HeapEntropyTracker, WeightedRandomPatternPicker), no backtracking, Cancel on failure, a
//! connected path constraint of colours with parity and count constraints. A port of the
//! game's IL, call by call and in the same order (collections enumerate as .NET's do), so a
//! seed lays out the tiles the game lays out. Settings the generators could have but do not
//! fail when read (wrecks.rs).

use std::collections::{HashMap, HashSet};

use indexmap::{IndexMap, IndexSet};

use super::unity::Id;
use crate::geometry::{matrix_mul, trs, Matrix};

/// FaceDetails of a Square face: the paint of its 3x3 cells (topLeft, top, topRight, left,
/// center, right, bottomLeft, bottom, bottomRight). The hex fields are 0 for cube tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FaceDetails {
    pub paint: [i32; 9],
}

/// SylvesOrientedFace: a face of one of a tile's cells.
#[derive(Clone, Debug)]
pub struct Face {
    pub offset: V3,
    /// Sylves CubeDir: Left, Right, Up, Down, Forward, Back.
    pub dir: u8,
    pub details: FaceDetails,
}

/// A TesseraTile (cube).
#[derive(Clone, Debug)]
pub struct TileDef {
    pub id: Id,
    pub name: String,
    pub palette: Option<Id>,
    pub faces: Vec<Face>,
    pub offsets: Vec<V3>,
    pub center: [f32; 3],
    pub cell_size: [f32; 3],
    pub rotatable: bool,
    pub reflectable: bool,
    /// RotationGroupType: None, XY, YZ, XZ, All.
    pub rotation_group: i32,
    pub symmetric: bool,
}

/// A TesseraConstraint on the generator.
#[derive(Clone, Debug)]
pub enum ConstraintDef {
    /// CountConstraint: comparison AtLeast, AtMost, Exactly.
    Count { tiles: Vec<Id>, comparison: i32, count: i32, eager: bool },
    /// PathConstraint with path colours, connected.
    Path { colors: Vec<i32>, parity: bool },
}

/// TesseraPalette.matchOverrides.
#[derive(Clone, Debug, Default)]
pub struct Palette {
    pub overrides: HashMap<(i32, i32), bool>,
}

/// A TesseraGenerator.
#[derive(Clone, Debug)]
pub struct Generator {
    pub size: V3,
    pub center: [f32; 3],
    pub cell_size: [f32; 3],
    pub retries: i32,
    pub search_initial_constraints: bool,
    /// TileEntry: tile and weight.
    pub tiles: Vec<(TileDef, f32)>,
    pub sky_box: Option<TileDef>,
    pub constraints: Vec<ConstraintDef>,
    pub palette: Palette,
}

impl Palette {
    /// TesseraPalette.Match(int, int).
    fn color(&self, a: i32, b: i32) -> bool {
        match self.overrides.get(&(a, b)) {
            Some(m) => *m,
            None => a == b || a == 0 || b == 0,
        }
    }

    /// TesseraPalette.Match(FaceDetails, FaceDetails): faces that touch, left against right.
    fn matches(&self, a: &FaceDetails, b: &FaceDetails) -> bool {
        let (a, b) = (&a.paint, &b.paint);
        [(0, 2), (1, 1), (2, 0), (3, 5), (4, 4), (5, 3), (6, 8), (7, 7), (8, 6)].iter().all(|&(i, j)| self.color(a[i], b[j])) && self.color(0, 0)
    }
}

impl FaceDetails {
    fn reflect_y(&mut self) {
        let p = &mut self.paint;
        p.swap(2, 8);
        p.swap(1, 7);
        p.swap(0, 6);
    }

    fn rotate_cw(&mut self) {
        let o = self.paint;
        let p = &mut self.paint;
        p[2] = o[0];
        p[8] = o[2];
        p[6] = o[8];
        p[0] = o[6];
        p[5] = o[1];
        p[7] = o[5];
        p[3] = o[7];
        p[1] = o[3];
    }
}

// ---- Sylves: cube cells, rotations and connections ---------------------------------------

pub type V3 = [i32; 3];

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> i32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// CubeRotation: the axis each basis vector goes to, in three nibbles (x, y, z), 8 for negated.
pub type Rot = i16;

const IDENTITY: Rot = 0x012;
const REFLECT_X: Rot = 0x812;
const ROTATE_XZ: Rot = 0x218;
const ROTATE_XY: Rot = 0x182;
const ROTATE_YZ: Rot = 0x029;

/// CubeDirExtensions.Forward.
fn forward(d: u8) -> V3 {
    match d {
        0 => [-1, 0, 0],
        1 => [1, 0, 0],
        2 => [0, 1, 0],
        3 => [0, -1, 0],
        4 => [0, 0, 1],
        _ => [0, 0, -1],
    }
}

/// CubeDirExtensions.Up.
fn up(d: u8) -> V3 {
    match d {
        2 | 3 => [0, 0, 1],
        _ => [0, 1, 0],
    }
}

/// CubeDirExtensions.Right.
fn right(d: u8) -> V3 {
    match d {
        0 => [0, 0, 1],
        1 => [0, 0, -1],
        2 | 5 => [-1, 0, 0],
        _ => [1, 0, 0],
    }
}

/// CubeRotation.ToDir.
fn to_dir(v: V3) -> u8 {
    match v {
        [1, _, _] => 1,
        [-1, _, _] => 0,
        [_, 1, _] => 2,
        [_, -1, _] => 3,
        [_, _, 1] => 4,
        _ => 5,
    }
}

/// CubeRotation * Vector3Int.
fn rot_apply(r: Rot, v: V3) -> V3 {
    let r = r as i32;
    let mut out = [0; 3];
    out[((r >> 8) & 3) as usize] = v[0] * if r & 0x800 != 0 { -1 } else { 1 };
    out[((r >> 4) & 3) as usize] = v[1] * if r & 0x80 != 0 { -1 } else { 1 };
    out[(r & 3) as usize] = v[2] * if r & 8 != 0 { -1 } else { 1 };
    out
}

/// CubeRotation * CubeDir.
fn rot_dir(r: Rot, d: u8) -> u8 {
    to_dir(rot_apply(r, forward(d)))
}

/// CubeRotation * CubeRotation.
fn rot_mul(a: Rot, b: Rot) -> Rot {
    let (a, b) = (a as i32, b as i32);
    let shift = |n: i32| ((2 - (n & 3)) << 2) & 31;
    let c0 = ((a >> shift(b >> 8)) & 15) ^ ((b >> 8) & 8);
    let c1 = ((a >> shift(b >> 4)) & 15) ^ ((b >> 4) & 8);
    let c2 = ((a >> shift(b)) & 15) ^ (b & 8);
    ((c0 << 8) + (c1 << 4) + c2) as Rot
}

/// CubeRotation.ToMatrix as integers: m[row][col], column j the image of basis vector j.
fn rot_matrix(r: Rot) -> [[i32; 3]; 3] {
    let mut m = [[0; 3]; 3];
    for (j, shift) in [8, 4, 0].into_iter().enumerate() {
        let n = (r as i32 >> shift) & 15;
        m[(n & 3) as usize][j] = if n & 8 != 0 { -1 } else { 1 };
    }
    m
}

/// CubeRotation.FromMatrixSimple.
fn from_matrix_simple(m: [[i32; 3]; 3]) -> Rot {
    let col = |j: usize| -> i32 {
        if m[0][j] != 0 {
            if m[0][j] > 0 { 0 } else { 8 }
        } else if m[1][j] != 0 {
            if m[1][j] > 0 { 1 } else { 9 }
        } else if m[2][j] > 0 {
            2
        } else {
            10
        }
    };
    ((col(0) << 8) | (col(1) << 4) | col(2)) as Rot
}

/// CubeRotation.FromMatrix of an integer matrix: each column must be a signed axis.
fn from_matrix(m: [[i32; 3]; 3]) -> Option<Rot> {
    (0..3).all(|j| (0..3).map(|i| m[i][j].abs()).sum::<i32>() == 1).then(|| from_matrix_simple(m))
}

fn mat_mul(a: [[i32; 3]; 3], b: [[i32; 3]; 3]) -> [[i32; 3]; 3] {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn transpose(m: [[i32; 3]; 3]) -> [[i32; 3]; 3] {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

/// CubeRotation.Invert.
fn rot_invert(r: Rot) -> Rot {
    from_matrix_simple(transpose(rot_matrix(r)))
}

/// CubeRotation.GetRotations: even permutations with even reflections, odd with odd, then
/// (with reflections) even permutations with odd reflections and odd with even.
fn cube_rotations(reflections: bool) -> Vec<Rot> {
    const EVEN_PERMUTATIONS: [i16; 3] = [0x012, 0x120, 0x201];
    const ODD_PERMUTATIONS: [i16; 3] = [0x021, 0x102, 0x210];
    const EVEN_REFLECTIONS: [i16; 4] = [0x000, 0x088, 0x808, 0x880];
    const ODD_REFLECTIONS: [i16; 4] = [0x008, 0x080, 0x800, 0x888];
    let mut out = Vec::new();
    let mut add = |rs: &[i16], ps: &[i16]| {
        for r in rs {
            for p in ps {
                out.push(p + r);
            }
        }
    };
    add(&EVEN_REFLECTIONS, &EVEN_PERMUTATIONS);
    add(&ODD_REFLECTIONS, &ODD_PERMUTATIONS);
    if reflections {
        add(&EVEN_REFLECTIONS, &ODD_PERMUTATIONS);
        add(&ODD_REFLECTIONS, &EVEN_PERMUTATIONS);
    }
    out
}

/// SylvesExtensions.GetRotations for cube tiles (CubeRotationsByGroupType: the group's
/// rotations, then the same with ReflectX first; the first half without reflections).
fn tile_rotations(t: &TileDef) -> Result<Vec<Rot>, String> {
    if !t.rotatable {
        return Ok(if t.reflectable { vec![IDENTITY, REFLECT_X] } else { vec![IDENTITY] });
    }
    let group = |g: Rot| {
        let mut v = vec![IDENTITY, g, rot_mul(g, g), rot_mul(rot_mul(g, g), g)];
        v.extend([rot_mul(REFLECT_X, IDENTITY), rot_mul(REFLECT_X, g), rot_mul(rot_mul(REFLECT_X, g), g), rot_mul(rot_mul(rot_mul(REFLECT_X, g), g), g)]);
        v
    };
    let all = match t.rotation_group {
        0 => vec![IDENTITY, REFLECT_X],
        1 => group(ROTATE_XY),
        2 => group(ROTATE_YZ),
        3 => group(ROTATE_XZ),
        4 => cube_rotations(true),
        g => return Err(format!("{}: rotation group {g}", t.name)),
    };
    let n = all.len();
    Ok(if t.reflectable { all } else { all[..n / 2].to_vec() })
}

/// Sylves Connection: how a face lines up after a move or a rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct Connection {
    mirror: bool,
    rotation: i32,
    sides: i32,
}

impl Connection {
    /// Connection * Connection.
    fn mul(self, b: Connection) -> Connection {
        let sides = if self.sides != 0 { self.sides } else { b.sides };
        let rotation = if sides == 0 {
            0
        } else if !self.mirror {
            (self.rotation + b.rotation) % sides
        } else {
            (sides + self.rotation - b.rotation) % sides
        };
        Connection { mirror: self.mirror ^ b.mirror, rotation, sides }
    }

    /// Connection.GetInverse (only rotation 0 is implemented in Sylves).
    fn inverse(self) -> Result<Connection, String> {
        if self.rotation != 0 {
            return Err("Connection.GetInverse of a rotated connection is not implemented".into());
        }
        Ok(Connection { mirror: self.mirror, ..Default::default() })
    }

    /// Connection.ToMatrix for a square face: Rotate(Euler(0, 0, 90 r)) * Scale(1, -1, 1 if mirrored).
    fn matrix(self) -> Result<[[i32; 3]; 3], String> {
        if self.sides != 4 {
            return Err(format!("Connection.ToMatrix with {} sides", self.sides));
        }
        let mirror = [[1, 0, 0], [0, if self.mirror { -1 } else { 1 }, 0], [0, 0, 1]];
        let rz = match self.rotation.rem_euclid(4) {
            0 => [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
            1 => [[0, -1, 0], [1, 0, 0], [0, 0, 1]],
            2 => [[-1, 0, 0], [0, -1, 0], [0, 0, 1]],
            _ => [[0, 1, 0], [-1, 0, 0], [0, 0, 1]],
        };
        Ok(mat_mul(rz, mirror))
    }
}

/// CubeCellType.Rotate(dir, rotation, out resultDir, out connection).
fn rotate_connection(dir: u8, r: Rot) -> (u8, Connection) {
    let result = rot_dir(r, dir);
    let ru = rot_apply(r, up(dir));
    let rr = rot_apply(r, right(dir));
    let mirror = dot(cross(rr, ru), forward(result)) < 0;
    let mut u = dot(rr, up(result));
    let rt = dot(rr, right(result));
    if mirror {
        u = -u;
    }
    // RoundToInt(Atan2(u, rt) / (pi / 2)) for the unit vectors these are.
    let k = match (u, rt) {
        (0, 1) => 0,
        (1, 0) => 1,
        (0, -1) => 2,
        _ => -1,
    };
    let rotation = if mirror { (-k + 4) % 4 } else { (k + 4) % 4 };
    (result, Connection { mirror, rotation, sides: 4 })
}

/// CubeCellType.TryGetRotation.
fn try_get_rotation(from: u8, to: u8, c: Connection) -> Result<Option<Rot>, String> {
    let frame = |d: u8| -> [[i32; 3]; 3] {
        let (r, u, f) = (right(d), up(d), forward(d));
        std::array::from_fn(|i| [r[i], u[i], f[i]])
    };
    Ok(from_matrix(mat_mul(mat_mul(frame(to), c.matrix()?), transpose(frame(from)))))
}

/// SylvesExtensions.RotateBy: a face's direction and paint under a rotation.
fn rotate_by(dir: u8, fd: &FaceDetails, r: Rot) -> (u8, FaceDetails) {
    let (d, c) = rotate_connection(dir, r);
    let mut fd = *fd;
    if c.mirror {
        fd.reflect_y();
    }
    for _ in 0..c.rotation {
        fd.rotate_cw();
    }
    (d, fd)
}

/// A CubeBound: min inclusive, max exclusive.
#[derive(Clone, Copy, Debug)]
struct Bound {
    min: V3,
    max: V3,
}

impl Bound {
    fn contains(&self, c: V3) -> bool {
        (0..3).all(|i| c[i] >= self.min[i] && c[i] < self.max[i])
    }
}

/// CubeGrid.FindBasicPath: x steps, then y, then z, each with the dir it moves in.
fn basic_path(start: V3, dest: V3) -> Vec<(V3, u8)> {
    let mut out = Vec::new();
    let mut c = start;
    for (axis, plus, minus) in [(0, 1u8, 0u8), (1, 2, 3), (2, 4, 5)] {
        while c[axis] < dest[axis] {
            out.push((c, plus));
            c[axis] += 1;
        }
        while c[axis] > dest[axis] {
            out.push((c, minus));
            c[axis] -= 1;
        }
    }
    out
}

/// DefaultGridImpl.ParallelTransport from an unbounded cube grid (a tile's cells) to a cube
/// grid with `bound` (None: unbounded): where aDest lands and the rotation there.
fn parallel_transport(a_src: V3, a_dest: V3, bound: Option<&Bound>, b_src: V3, start: Rot) -> Result<Option<(V3, Rot)>, String> {
    let inside = |c: V3| bound.is_none_or(|b| b.contains(c));
    if !inside(b_src) {
        return Ok(None);
    }
    let (mut b, mut rot) = (b_src, start);
    for (_, dir) in basic_path(a_src, a_dest) {
        let a_inverse = dir ^ 1;
        let (b_dir, b_conn) = rotate_connection(dir, rot);
        b = add(b, forward(b_dir));
        if !inside(b) {
            return Ok(None);
        }
        let mirror = Connection { mirror: true, ..Default::default() };
        let s = mirror.mul(b_conn).mul(mirror);
        let t = Connection::default().mul(s).mul(Connection::default().inverse()?);
        match try_get_rotation(a_inverse, b_dir ^ 1, t)? {
            Some(r) => rot = r,
            None => return Ok(None),
        }
    }
    Ok(Some((b, rot)))
}

// ---- Tessera: the tile model ----------------------------------------------------------------

/// Tessera ModelTile: one cell of one rotation of a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ModelTile {
    tile: usize,
    rot: Rot,
    offset: V3,
}

/// TileModelInfo.InternalAdjacency.
struct Internal {
    src: ModelTile,
    dest: ModelTile,
    dir: u8,
}

/// TileModelInfo, the AdjacentModel built from it, and what the generators' constraints and
/// output need of it.
pub struct Model {
    pub generator: Generator,
    /// Distinct tiles (by component), the generator's entries index into it.
    pub tiles: Vec<TileDef>,
    internal: Vec<Internal>,
    by_dir: [Vec<(FaceDetails, ModelTile)>; 6],
    /// AdjacentModel: pattern per model tile, in SetFrequency order.
    patterns: IndexMap<ModelTile, usize>,
    frequencies: Vec<f64>,
    plogp: Vec<f64>,
    /// propagator[pattern][direction]: the patterns allowed next to it there.
    propagator: Vec<[Vec<u16>; 6]>,
    /// Which (pattern, direction, pattern) adjacencies propagator holds, as bits.
    adjacent: Vec<u64>,
    sky_box: Option<[FaceDetails; 6]>,
}

/// SylvesDeBroglieUtils.CubeMapping: CellDir to DeBroglie Direction (XPlus, XMinus, YPlus, ...).
const DIRECTION: [usize; 6] = [1, 0, 2, 3, 4, 5];

impl Model {
    /// TileModelInfo.Create and SylvesDeBroglieUtils.GetAdjacentPaintTileModel.
    pub fn new(generator: Generator) -> Result<Model, String> {
        let mut tiles: Vec<TileDef> = Vec::new();
        let mut entries = Vec::new();
        for (t, w) in &generator.tiles {
            let i = match tiles.iter().position(|x| x.id == t.id) {
                Some(i) => i,
                None => {
                    tiles.push(t.clone());
                    tiles.len() - 1
                }
            };
            entries.push((i, *w));
        }
        // Canonicalization: each model tile to the one of the first paint-equivalent rotation.
        let mut canon: HashMap<ModelTile, ModelTile> = HashMap::new();
        for &(ti, _) in &entries {
            let t = &tiles[ti];
            let rotations = tile_rotations(t)?;
            let mut seen = HashSet::new();
            for &r in &rotations {
                if t.symmetric {
                    for &r2 in &rotations {
                        if seen.contains(&r2) {
                            continue;
                        }
                        if let Some(realign) = paint_equivalent(t, r, r2)? {
                            for (k, v) in realign {
                                canon.insert(ModelTile { tile: ti, rot: r2, offset: v }, ModelTile { tile: ti, rot: r, offset: k });
                            }
                            seen.insert(r2);
                        }
                    }
                } else {
                    for &o in &t.offsets {
                        let m = ModelTile { tile: ti, rot: r, offset: o };
                        canon.insert(m, m);
                    }
                }
            }
        }
        let canonical: HashSet<ModelTile> = canon.values().copied().collect();
        let mut uncanonical: HashMap<ModelTile, usize> = HashMap::new();
        for v in canon.values() {
            *uncanonical.entry(*v).or_default() += 1;
        }
        let mut all: Vec<(ModelTile, f32)> = Vec::new();
        let mut internal = Vec::new();
        let mut by_dir: [Vec<(FaceDetails, ModelTile)>; 6] = Default::default();
        for &(ti, weight) in &entries {
            let t = &tiles[ti];
            if !contiguous(t) {
                eprintln!("read-game: Tessera cannot use {} as it is not contiguous", t.name);
                continue;
            }
            for rot in tile_rotations(t)? {
                for &o in &t.offsets {
                    let m = ModelTile { tile: ti, rot, offset: o };
                    if !canonical.contains(&m) {
                        continue;
                    }
                    all.push((m, weight * uncanonical[&m] as f32 / t.offsets.len() as f32));
                    if t.offsets.len() > 1 {
                        for dir in 0..6 {
                            let dest = add(o, forward(dir));
                            if !t.offsets.contains(&dest) {
                                continue;
                            }
                            let d = ModelTile { tile: ti, rot, offset: dest };
                            if canonical.contains(&d) {
                                internal.push(Internal { src: m, dest: d, dir: rot_dir(rot, dir) });
                            }
                        }
                    }
                }
                for f in &t.faces {
                    let m = ModelTile { tile: ti, rot, offset: f.offset };
                    if canonical.contains(&m) {
                        let (d, fd) = rotate_by(f.dir, &f.details, rot);
                        by_dir[d as usize].push((fd, m));
                    }
                }
            }
        }
        // AdjacentModel: SetFrequency in AllTiles order makes the patterns; the internal
        // adjacencies, then the painted ones of each pair of opposite directions.
        let mut model = Model { generator, tiles, internal, by_dir, patterns: IndexMap::new(), frequencies: Vec::new(), plogp: Vec::new(), propagator: Vec::new(), adjacent: Vec::new(), sky_box: None };
        for (m, f) in &all {
            let p = model.pattern(*m);
            model.frequencies[p] = *f as f64;
        }
        if model.patterns.len() > u16::MAX as usize + 1 {
            return Err(format!("{} patterns: more than the port's 65536", model.patterns.len()));
        }
        let pc = model.patterns.len();
        model.adjacent = vec![0; (pc * 6 * pc).div_ceil(64)];
        // Every tile here is canonical, so has its pattern already.
        let internal: Vec<(usize, usize, usize)> = model.internal.iter().map(|a| (model.patterns[&a.src], model.patterns[&a.dest], DIRECTION[a.dir as usize])).collect();
        for (s, d, dir) in internal {
            model.add_adjacency(s, d, dir);
        }
        // Palette.Match of each pair of distinct paints, then the pairs in order.
        let mut paints: IndexSet<FaceDetails> = IndexSet::new();
        let faces: [Vec<(usize, usize)>; 6] = std::array::from_fn(|d| model.by_dir[d].iter().map(|(fd, t)| (paints.insert_full(*fd).0, model.patterns[t])).collect());
        let table: Vec<Vec<bool>> = paints.iter().map(|a| paints.iter().map(|b| model.generator.palette.matches(a, b)).collect()).collect();
        for (d1, d2) in [(0usize, 1usize), (2, 3), (4, 5)] {
            for &(f1, t1) in &faces[d1] {
                for &(f2, t2) in &faces[d2] {
                    if table[f1][f2] {
                        model.add_adjacency(t1, t2, DIRECTION[d1]);
                    }
                }
            }
        }
        if model.patterns.len() > u16::MAX as usize + 1 {
            return Err(format!("{} patterns: more than the port's 65536", model.patterns.len()));
        }
        if model.frequencies.iter().sum::<f64>() == 0.0 {
            return Err("No tiles have assigned frequences.".into());
        }
        model.plogp = model.frequencies.iter().map(|&f| if f > 0.0 { f * f.ln() } else { 0.0 }).collect();
        if let Some(sky) = &model.generator.sky_box {
            let mut faces = [FaceDetails { paint: [0; 9] }; 6];
            for (x, face) in faces.iter_mut().enumerate() {
                let x = x as u8;
                *face = sky.faces.iter().find(|f| f.dir == x).or_else(|| sky.faces.iter().find(|f| f.dir == x ^ 1)).ok_or(format!("Couldn't find skybox for direction {x}"))?.details;
            }
            model.sky_box = Some(faces);
        }
        Ok(model)
    }

    /// Debug: the model's size.
    pub fn stats(&self) -> String {
        let adjacencies: usize = self.propagator.iter().map(|p| p.iter().map(Vec::len).sum::<usize>()).sum();
        format!("{} tiles, {} patterns, {} adjacencies", self.tiles.len(), self.patterns.len(), adjacencies)
    }

    /// AdjacentModel.GetPattern.
    fn pattern(&mut self, m: ModelTile) -> usize {
        if let Some(p) = self.patterns.get(&m) {
            return *p;
        }
        let p = self.patterns.len();
        self.patterns.insert(m, p);
        self.frequencies.push(0.0);
        self.propagator.push(Default::default());
        p
    }

    /// AdjacentModel.AddAdjacency of two patterns: both ways, each list keeping its first
    /// insertion order.
    fn add_adjacency(&mut self, s: usize, t: usize, d: usize) {
        let pc = self.patterns.len();
        for (a, dir, b) in [(s, d, t), (t, d ^ 1, s)] {
            let bit = (a * 6 + dir) * pc + b;
            if self.adjacent[bit / 64] & (1 << (bit % 64)) == 0 {
                self.adjacent[bit / 64] |= 1 << (bit % 64);
                self.propagator[a][dir].push(b as u16);
            }
        }
    }

    /// TileModelMapping.CreateTileSet: the patterns of the tiles, in the tiles' order.
    fn tile_set(&self, tiles: impl IntoIterator<Item = ModelTile>) -> TileSet {
        let mut patterns = IndexSet::new();
        for t in tiles {
            if let Some(p) = self.patterns.get(&t) {
                patterns.insert(*p);
            }
        }
        let mut member = vec![false; self.patterns.len()];
        for p in &patterns {
            member[*p] = true;
        }
        TileSet { patterns: patterns.into_iter().collect(), member }
    }

    /// The model tiles of tiles (TesseraConstraint.GetModelTiles) at their first offset.
    fn first_offsets(&self, ids: &[Id]) -> Result<Vec<ModelTile>, String> {
        let mut out = Vec::new();
        for id in ids {
            // A tile the generator does not use has no patterns.
            let Some(ti) = self.tiles.iter().position(|t| t.id == *id) else { continue };
            let t = &self.tiles[ti];
            for rot in tile_rotations(t)? {
                out.push(ModelTile { tile: ti, rot, offset: t.offsets[0] });
            }
        }
        Ok(out)
    }

    /// PathConstraint.GetTileConstraint: the path exits of each model tile (faces painted a
    /// path colour, and the inside of path tiles).
    fn exits(&self, colors: &[i32]) -> IndexMap<ModelTile, IndexSet<usize>> {
        let mut exits: IndexMap<ModelTile, IndexSet<usize>> = IndexMap::new();
        for (dir, list) in self.by_dir.iter().enumerate() {
            for (fd, t) in list {
                if colors.contains(&fd.paint[4]) {
                    exits.entry(*t).or_default().insert(DIRECTION[dir]);
                }
            }
        }
        let path_tiles: HashSet<usize> = exits.keys().map(|t| t.tile).collect();
        for a in &self.internal {
            if path_tiles.contains(&a.src.tile) {
                exits.entry(a.src).or_default().insert(DIRECTION[a.dir as usize]);
            }
        }
        exits
    }
}

/// TileModelInfo.IsPaintEquivalent: whether rotation r2 of a tile paints as r1 does, and
/// where each face's cell goes.
fn paint_equivalent(t: &TileDef, r1: Rot, r2: Rot) -> Result<Option<IndexMap<V3, V3>>, String> {
    let rotation = rot_mul(rot_invert(r1), r2);
    // CubeGrid.FindGridSymmetry of the tile's cells onto themselves.
    let min = t.offsets.iter().fold(t.offsets[0], |a, b| std::array::from_fn(|i| a[i].min(b[i])));
    let max = t.offsets.iter().fold(t.offsets[0], |a, b| std::array::from_fn(|i| a[i].max(b[i])));
    let (a, b) = (rot_apply(rotation, min), rot_apply(rotation, max));
    let rmin: V3 = std::array::from_fn(|i| a[i].min(b[i]));
    let translation = sub(min, rmin);
    if !t.offsets.iter().all(|c| t.offsets.contains(&add(translation, rot_apply(rotation, *c)))) {
        return Ok(None);
    }
    let mut realign = IndexMap::new();
    for f in &t.faces {
        let dest = add(translation, rot_apply(rotation, f.offset));
        realign.insert(f.offset, dest);
        let (d, fd) = rotate_by(f.dir, &f.details, rotation);
        let other = t.faces.iter().filter(|g| g.offset == dest && g.dir == d).collect::<Vec<_>>();
        let [other] = other[..] else { return Err(format!("Couldn't find face at offset {dest:?} in direction {d} of {}", t.name)) };
        if fd != other.details {
            return Ok(None);
        }
    }
    Ok(Some(realign))
}

/// TileModelInfo.IsContiguous.
fn contiguous(t: &TileDef) -> bool {
    if t.offsets.len() == 1 {
        return true;
    }
    let mut left: HashSet<V3> = t.offsets.iter().copied().collect();
    let mut stack = vec![t.offsets[0]];
    while let Some(c) = stack.pop() {
        left.remove(&c);
        for d in 0..6 {
            let n = add(c, forward(d));
            if left.contains(&n) {
                stack.push(n);
            }
        }
    }
    left.is_empty()
}

// ---- Tessera.XoRoRNG ---------------------------------------------------------------------

/// Tessera.XoRoRNG: xoroshiro128+ seeded by splitmix64.
pub struct XoRo {
    s0: u64,
    s1: u64,
}

impl XoRo {
    pub fn new(seed: i64) -> XoRo {
        let mut z = (seed as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mix = |mut z: u64| {
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        let s0 = mix(z);
        z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
        XoRo { s0, s1: mix(z) }
    }

    fn next_long(&mut self) -> u64 {
        let (s0, mut s1) = (self.s0, self.s1);
        let result = s0.wrapping_add(s1);
        s1 ^= s0;
        self.s0 = s0.rotate_left(55) ^ s1 ^ (s1 << 14);
        self.s1 = s1.rotate_left(36);
        result
    }

    /// NextDouble: the low 53 bits.
    pub fn next_double(&mut self) -> f64 {
        (self.next_long() & 0x1f_ffff_ffff_ffff) as f64 * 1.1102230246251565e-16
    }
}

// ---- DeBroglie ---------------------------------------------------------------------------

const UNDECIDED: i32 = -1;
const CONTRADICTION: i32 = -2;
const DECIDED: i32 = 0;

/// TilePropagatorTileSet as patterns: in the tiles' order, and as a membership table.
#[derive(Clone)]
struct TileSet {
    patterns: Vec<usize>,
    member: Vec<bool>,
}

/// ITracker: SelectedTracker, or SelectedChangeTracker with CountConstraint.CountTracker.
enum Tracker {
    Selected { member: Vec<bool>, counts: Vec<i32> },
    Changes { member: Vec<bool>, counts: Vec<i32>, values: Vec<i32>, no: i32, yes: i32, maybe: i32 },
}

/// TilePropagator with its WavePropagator, Wave, Ac4PatternModelConstraint and
/// HeapEntropyTracker, over a GridTopology with a mask.
struct Wfc<'m> {
    m: &'m Model,
    size: [usize; 3],
    mask: Option<Vec<bool>>,
    n: usize,
    pc: usize,
    possible: Vec<bool>,
    count: Vec<i32>,
    /// compatible[(cell * 6 + direction) * patterns + pattern].
    compatible: Vec<i32>,
    /// GridTopology.TryMove of each cell and direction (usize::MAX: none).
    neighbours: Vec<[usize; 6]>,
    to_propagate: Vec<(usize, usize)>,
    status: i32,
    deferred: bool,
    sum: Vec<f64>,
    plogp_sum: Vec<f64>,
    tiebreaker: Vec<f64>,
    trackers: Vec<Tracker>,
}

impl<'m> Wfc<'m> {
    /// new TilePropagator(model, topology, options): WavePropagator.Clear up to its constraints.
    fn new(m: &'m Model, size: [usize; 3], mask: Option<Vec<bool>>, rng: &mut XoRo) -> Wfc<'m> {
        let n = size[0] * size[1] * size[2];
        let pc = m.patterns.len();
        let mut w = Wfc {
            m,
            size,
            mask,
            n,
            pc,
            possible: vec![true; n * pc],
            count: vec![pc as i32; n],
            compatible: vec![0; n * pc * 6],
            neighbours: Vec::new(),
            to_propagate: Vec::new(),
            status: UNDECIDED,
            deferred: false,
            sum: vec![0.0; n],
            plogp_sum: vec![0.0; n],
            tiebreaker: vec![0.0; n],
            trackers: Vec::new(),
        };
        w.neighbours = (0..n).map(|i| std::array::from_fn(|d| w.try_move(i, d).unwrap_or(usize::MAX))).collect();
        // HeapEntropyTracker.Reset: the sums of a full cell, a tiebreaker per cell.
        let (mut plogp, mut sum) = (0.0f64, 0.0f64);
        for p in 0..pc {
            plogp += m.plogp[p];
            sum += m.frequencies[p];
        }
        for i in 0..n {
            if w.contains(i) {
                w.plogp_sum[i] = plogp;
                w.sum[i] = sum;
                w.tiebreaker[i] = rng.next_double() * 1e-10;
            }
        }
        // Ac4PatternModelConstraint.Clear: supports, and bans of patterns without any.
        for i in 0..n {
            if !w.contains(i) {
                continue;
            }
            let neighbours: [Option<usize>; 6] = std::array::from_fn(|d| w.try_move(i, d).map(|_| d));
            for p in 0..pc {
                for (d, el) in neighbours.iter().enumerate() {
                    let Some(el) = el else { continue };
                    let len = m.propagator[p][*el].len() as i32;
                    w.compatible[(i * 6 + d) * pc + p] = len;
                    if len == 0 && w.get(i, p) {
                        if w.internal_ban(i, p) {
                            w.status = CONTRADICTION;
                        }
                        break;
                    }
                }
            }
        }
        w
    }

    fn contains(&self, i: usize) -> bool {
        self.mask.as_ref().is_none_or(|m| m[i])
    }

    fn index(&self, c: V3) -> usize {
        c[0] as usize + c[1] as usize * self.size[0] + c[2] as usize * self.size[0] * self.size[1]
    }

    fn coord(&self, i: usize) -> V3 {
        let (w, h) = (self.size[0], self.size[1]);
        [(i % w) as i32, (i / w % h) as i32, (i / w / h) as i32]
    }

    /// GridTopology.TryMove (direction = DeBroglie Direction).
    fn try_move(&self, i: usize, d: usize) -> Option<usize> {
        const DX: [[i32; 3]; 6] = [[1, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [0, 0, 1], [0, 0, -1]];
        let c = add(self.coord(i), DX[d]);
        if (0..3).any(|k| c[k] < 0 || c[k] >= self.size[k] as i32) {
            return None;
        }
        let j = self.index(c);
        self.contains(j).then_some(j)
    }

    fn get(&self, i: usize, p: usize) -> bool {
        self.possible[i * self.pc + p]
    }

    /// WavePropagator.InternalBan: true when the cell has no pattern left.
    fn internal_ban(&mut self, i: usize, p: usize) -> bool {
        let pc = self.pc;
        for d in 0..6 {
            self.compatible[(i * 6 + d) * pc + p] -= pc as i32;
        }
        self.to_propagate.push((i, p));
        self.possible[i * pc + p] = false;
        self.count[i] -= 1;
        let empty = self.count[i] == 0;
        self.plogp_sum[i] -= self.m.plogp[p];
        self.sum[i] -= self.m.frequencies[p];
        let count = self.count[i];
        for t in &mut self.trackers {
            match t {
                Tracker::Selected { member, counts } => {
                    if member[p] {
                        counts[i] -= 1;
                    }
                }
                Tracker::Changes { member, counts, values, no, yes, maybe } => {
                    if member[p] {
                        counts[i] -= 1;
                    }
                    let q = quadstate_changes(counts[i], count);
                    if q != values[i] {
                        let before = values[i];
                        values[i] = q;
                        notify(before, q, no, yes, maybe);
                    }
                }
            }
        }
        empty
    }

    /// Ac4PatternModelConstraint.Propagate.
    fn propagate(&mut self) {
        while let Some((i, p)) = self.to_propagate.pop() {
            let m = self.m;
            for d in 0..6 {
                let i2 = self.neighbours[i][d];
                if i2 == usize::MAX {
                    continue;
                }
                let base = (i2 * 6 + (d ^ 1)) * self.pc;
                for &q in &m.propagator[p][d] {
                    let k = base + q as usize;
                    self.compatible[k] -= 1;
                    if self.compatible[k] == 0 && self.internal_ban(i2, q as usize) {
                        self.status = CONTRADICTION;
                    }
                }
            }
            if self.status == CONTRADICTION {
                return;
            }
        }
    }

    /// WavePropagator.Ban.
    fn ban_pattern(&mut self, i: usize, p: usize) -> i32 {
        if self.get(i, p) {
            self.deferred = true;
            if self.internal_ban(i, p) {
                self.status = CONTRADICTION;
                return self.status;
            }
        }
        self.propagate();
        self.status
    }

    /// TilePropagator.Ban(x, y, z, tileSet).
    fn ban(&mut self, i: usize, set: &TileSet) -> i32 {
        for &p in &set.patterns {
            let r = self.ban_pattern(i, p);
            if r != UNDECIDED {
                return r;
            }
        }
        UNDECIDED
    }

    /// TilePropagator.Select(x, y, z, tileSet).
    fn select(&mut self, i: usize, set: &TileSet) -> i32 {
        for p in 0..self.pc {
            if !set.member[p] {
                let r = self.ban_pattern(i, p);
                if r != UNDECIDED {
                    return r;
                }
            }
        }
        UNDECIDED
    }

    /// CreateSelectedTracker / CreateSelectedChangeTracker: counted from the wave now.
    fn add_tracker(&mut self, set: &TileSet, changes: bool) -> usize {
        let mut counts = vec![0; self.n];
        for (i, c) in counts.iter_mut().enumerate() {
            if self.contains(i) {
                *c = set.patterns.iter().filter(|p| self.get(i, **p)).count() as i32;
            }
        }
        let t = if changes {
            let mut values = vec![0; self.n];
            let (mut no, mut yes, mut maybe) = (0, 0, 0);
            for i in 0..self.n {
                if self.contains(i) {
                    values[i] = quadstate_changes(counts[i], self.count[i]);
                }
            }
            // CountTracker.Reset.
            for i in 0..self.n {
                if self.contains(i) {
                    match values[i] {
                        -1 => no += 1,
                        0 => maybe += 1,
                        1 => yes += 1,
                        _ => {}
                    }
                }
            }
            Tracker::Changes { member: set.member.clone(), counts, values, no, yes, maybe }
        } else {
            Tracker::Selected { member: set.member.clone(), counts }
        };
        self.trackers.push(t);
        self.trackers.len() - 1
    }

    /// SelectedTracker.GetQuadstate / SelectedChangeTracker.GetQuadstate.
    fn quadstate(&self, t: usize, i: usize) -> i32 {
        match &self.trackers[t] {
            Tracker::Selected { counts, .. } => {
                if counts[i] == 0 {
                    -1
                } else if self.count[i] == counts[i] {
                    1
                } else {
                    0
                }
            }
            Tracker::Changes { values, .. } => values[i],
        }
    }

    fn counts(&self, t: usize) -> (i32, i32) {
        match &self.trackers[t] {
            Tracker::Changes { yes, maybe, .. } => (*yes, *maybe),
            Tracker::Selected { .. } => (0, 0),
        }
    }

    /// HeapEntropyTracker.GetRandomIndex: the lowest entropy plus tiebreaker among the cells
    /// with a choice left (what the heap's top is).
    fn pick_index(&self) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for i in 0..self.n {
            if !self.contains(i) || self.count[i] <= 1 {
                continue;
            }
            let key = (self.sum[i].ln() - self.plogp_sum[i] / self.sum[i]) + self.tiebreaker[i];
            if best.is_none_or(|(_, k)| key < k) {
                best = Some((i, key));
            }
        }
        best.map(|b| b.0)
    }

    /// RandomPickerUtils.GetRandomPossiblePattern.
    fn pick_pattern(&self, i: usize, rng: &mut XoRo) -> usize {
        let f = &self.m.frequencies;
        let mut s = 0.0;
        for p in 0..self.pc {
            if self.get(i, p) {
                s += f[p];
            }
        }
        let mut r = rng.next_double() * s;
        for p in 0..self.pc {
            if self.get(i, p) {
                r -= f[p];
            }
            if r <= 0.0 {
                return p;
            }
        }
        self.pc - 1
    }

    /// WavePropagator.StepConstraints.
    fn step_constraints(&mut self, cs: &mut [Constraint]) -> Result<(), String> {
        for c in cs.iter_mut() {
            c.check(self)?;
            if self.status != UNDECIDED {
                return Ok(());
            }
            self.propagate();
            if self.status != UNDECIDED {
                return Ok(());
            }
        }
        self.deferred = false;
        Ok(())
    }

    /// WavePropagator.Step (no backtracking).
    fn step(&mut self, cs: &mut [Constraint], rng: &mut XoRo) -> Result<(), String> {
        if self.deferred {
            self.step_constraints(cs)?;
        }
        if self.status != UNDECIDED {
            return Ok(());
        }
        let index = self.pick_index();
        if let Some(i) = index {
            let chosen = self.pick_pattern(i, rng);
            for p in 0..self.pc {
                if p != chosen && self.get(i, p) && self.internal_ban(i, p) {
                    self.status = CONTRADICTION;
                    break;
                }
            }
        }
        if self.status == UNDECIDED {
            self.propagate();
        }
        if self.status == UNDECIDED {
            self.step_constraints(cs)?;
        }
        if index.is_none() && self.status == UNDECIDED {
            self.status = DECIDED;
        }
        Ok(())
    }

    /// WavePropagator.GetDecidedPattern.
    fn decided(&self, i: usize) -> Option<usize> {
        let mut found = None;
        for p in 0..self.pc {
            if self.get(i, p) {
                if found.is_some() {
                    return None;
                }
                found = Some(p);
            }
        }
        found
    }
}

/// SelectedChangeTracker.GetQuadstateInner: Contradiction, No, Yes or Maybe.
fn quadstate_changes(count: i32, wave: i32) -> i32 {
    if wave == 0 {
        -2
    } else if count == 0 {
        -1
    } else if wave == count {
        1
    } else {
        0
    }
}

/// CountConstraint.CountTracker.Notify.
fn notify(before: i32, after: i32, no: &mut i32, yes: &mut i32, maybe: &mut i32) {
    match before {
        -1 => *no -= 1,
        0 => *maybe -= 1,
        1 => *yes -= 1,
        _ => {}
    }
    match after {
        -1 => *no += 1,
        0 => *maybe += 1,
        1 => *yes += 1,
        _ => {}
    }
}

/// EdgedPathView: a node per cell and per cell exit (7 a cell), with its trackers.
struct PathView {
    path: TileSet,
    path_tracker: usize,
    /// tileSetByExit and trackerByExit: (direction, tiles, tracker).
    exits: Vec<(usize, TileSet, usize)>,
    neighbours: Vec<Vec<usize>>,
    could: Vec<bool>,
    must: Vec<bool>,
}

impl PathView {
    /// new EdgedPathView(spec, propagator) without rotations or end points.
    fn new(w: &mut Wfc, exits: &IndexMap<ModelTile, IndexSet<usize>>) -> PathView {
        let path = w.m.tile_set(exits.keys().copied());
        let path_tracker = w.add_tracker(&path, false);
        let mut neighbours = vec![Vec::new(); w.n * 7];
        for i in 0..w.n {
            if !w.contains(i) {
                continue;
            }
            neighbours[i * 7] = (0..6).map(|d| i * 7 + 1 + d).collect();
            for d in 0..6 {
                neighbours[i * 7 + 1 + d] = match w.try_move(i, d) {
                    Some(j) => vec![i * 7, j * 7 + 1 + (d ^ 1)],
                    None => vec![i * 7],
                };
            }
        }
        // GroupBy direction over (tile, exit) in the exits' order.
        let mut groups: IndexMap<usize, Vec<ModelTile>> = IndexMap::new();
        for (t, dirs) in exits {
            for d in dirs {
                groups.entry(*d).or_default().push(*t);
            }
        }
        let mut by_exit = Vec::new();
        for (d, tiles) in groups {
            let set = w.m.tile_set(tiles);
            let tracker = w.add_tracker(&set, false);
            by_exit.push((d, set, tracker));
        }
        PathView { path, path_tracker, exits: by_exit, neighbours, could: vec![false; w.n * 7], must: vec![false; w.n * 7] }
    }

    fn update(&mut self, w: &Wfc) {
        for (d, _, t) in &self.exits {
            for i in 0..w.n {
                let q = w.quadstate(*t, i);
                self.could[i * 7 + 1 + d] = q >= 0;
                self.must[i * 7 + 1 + d] = q == 1;
            }
        }
        for i in 0..w.n {
            let q = w.quadstate(self.path_tracker, i);
            self.could[i * 7] = q >= 0;
            self.must[i * 7] = q == 1;
        }
    }

    fn exit(&self, d: usize) -> Option<&(usize, TileSet, usize)> {
        self.exits.iter().find(|e| e.0 == d)
    }

    fn select_path(&self, w: &mut Wfc, node: usize) {
        let (i, d) = (node / 7, node % 7);
        if d == 0 {
            w.select(i, &self.path);
            return;
        }
        if self.must[node] {
            return;
        }
        if let Some((_, set, _)) = self.exit(d - 1) {
            w.select(i, set);
        }
    }

    fn ban_relevant(&self, w: &mut Wfc, node: usize) {
        if node.is_multiple_of(7) {
            w.ban(node / 7, &self.path);
        }
    }
}

/// PathConstraintUtils.GetArticulationPoints: (articulation, component, component count).
fn articulation_points(neighbours: &[Vec<usize>], walkable: &[bool], relevant: &[bool]) -> (Vec<bool>, Vec<Option<usize>>, usize) {
    let n = walkable.len();
    let mut low = vec![0usize; n];
    let mut dfs = vec![0usize; n];
    let mut num = 1;
    let mut articulation = vec![false; n];
    let mut component: Vec<Option<usize>> = vec![None; n];
    let mut current = 0;
    struct Frame {
        u: usize,
        state: u8,
        next: usize,
        relevant_children: usize,
    }
    for start in 0..n {
        if !walkable[start] || !relevant[start] || dfs[start] != 0 {
            continue;
        }
        // CutVertex, iteratively.
        let mut frames = vec![Frame { u: start, state: 0, next: 0, relevant_children: 0 }];
        let mut child_relevant = false;
        let result = loop {
            let top = frames.len() - 1;
            let u = frames[top].u;
            if frames[top].state == 0 {
                component[u] = Some(current);
                dfs[u] = num;
                low[u] = num;
                num += 1;
                frames[top].state = 1;
            } else if frames[top].state == 2 {
                let v = neighbours[u][frames[top].next];
                if child_relevant {
                    frames[top].relevant_children += 1;
                }
                if low[v] >= dfs[u] && child_relevant {
                    articulation[u] = true;
                }
                low[u] = low[u].min(low[v]);
                frames[top].next += 1;
                frames[top].state = 1;
            }
            // State 1: the next neighbour.
            let mut pushed = false;
            while frames[top].next < neighbours[u].len() {
                let v = neighbours[u][frames[top].next];
                if !walkable[v] {
                    frames[top].next += 1;
                    continue;
                }
                if dfs[v] == 0 {
                    frames[top].state = 2;
                    frames.push(Frame { u: v, state: 0, next: 0, relevant_children: 0 });
                    pushed = true;
                    break;
                }
                low[u] = low[u].min(dfs[v]);
                frames[top].next += 1;
            }
            if pushed {
                continue;
            }
            // State 3: done with u.
            if top == 0 {
                break frames[0].relevant_children;
            }
            child_relevant = frames[top].relevant_children > 0 || relevant[u];
            frames.pop();
        };
        articulation[start] = result > 1;
        current += 1;
    }
    (articulation, component, current)
}

/// The DeBroglie constraints the generators use, with what Init made.
enum Constraint {
    Count { set: TileSet, tracker: usize, comparison: i32, count: i32, eager: bool },
    Connected { exits: IndexMap<ModelTile, IndexSet<usize>>, view: Option<PathView> },
    Parity { exits: IndexMap<ModelTile, IndexSet<usize>>, odd: Option<(TileSet, Option<usize>)>, view: Option<PathView> },
}

impl Constraint {
    fn init(&mut self, w: &mut Wfc, rng: &mut XoRo) -> Result<(), String> {
        match self {
            Constraint::Count { set, tracker, comparison, count, eager } => {
                *tracker = w.add_tracker(set, true);
                if !*eager {
                    return Ok(());
                }
                for _ in 0..1_000_000 {
                    let mut yes = 0;
                    let mut maybes = Vec::new();
                    for i in 0..w.n {
                        if !w.contains(i) {
                            continue;
                        }
                        match w.quadstate(*tracker, i) {
                            0 => maybes.push(i),
                            1 => yes += 1,
                            _ => {}
                        }
                    }
                    if *comparison == 1 {
                        if yes > *count {
                            w.status = CONTRADICTION;
                            return Ok(());
                        }
                        if yes == *count || maybes.is_empty() {
                            return self.check(w);
                        }
                        let i = maybes[(rng.next_double() * maybes.len() as f64) as i32 as usize];
                        w.select(i, set);
                    } else {
                        if yes + (maybes.len() as i32) < *count {
                            w.status = CONTRADICTION;
                            return Ok(());
                        }
                        if yes + maybes.len() as i32 == *count || maybes.is_empty() {
                            return self.check(w);
                        }
                        let i = maybes[(rng.next_double() * maybes.len() as f64) as i32 as usize];
                        w.ban(i, set);
                    }
                }
                Err("eager CountConstraint did not settle".into())
            }
            Constraint::Connected { exits, view } => {
                let mut v = PathView::new(w, exits);
                v.update(w);
                for node in 0..v.must.len() {
                    if v.must[node] {
                        v.select_path(w, node);
                    }
                }
                *view = Some(v);
                Ok(())
            }
            Constraint::Parity { exits, odd, view } => {
                let tiles: Vec<ModelTile> = exits.iter().filter(|(_, d)| d.len() % 2 == 1).map(|(t, _)| *t).collect();
                let set = w.m.tile_set(tiles.iter().copied());
                let tracker = (!tiles.is_empty()).then(|| w.add_tracker(&set, false));
                *odd = Some((set, tracker));
                *view = Some(PathView::new(w, exits));
                Ok(())
            }
        }
    }

    fn check(&mut self, w: &mut Wfc) -> Result<(), String> {
        match self {
            Constraint::Count { set, tracker, comparison, count, .. } => {
                let (yes, maybe) = w.counts(*tracker);
                if *comparison == 1 || *comparison == 2 {
                    if yes > *count {
                        w.status = CONTRADICTION;
                        return Ok(());
                    }
                    if yes == *count && maybe > 0 {
                        for i in 0..w.n {
                            if w.contains(i) && w.quadstate(*tracker, i) == 0 {
                                w.ban(i, set);
                            }
                        }
                    }
                }
                if *comparison == 0 || *comparison == 2 {
                    // The counts read before the bans above, as CountConstraint.Check keeps them.
                    if yes + maybe < *count {
                        w.status = CONTRADICTION;
                        return Ok(());
                    }
                    if yes + maybe == *count && maybe > 0 {
                        for i in 0..w.n {
                            if w.contains(i) && w.quadstate(*tracker, i) == 0 {
                                w.select(i, set);
                            }
                        }
                    }
                }
                Ok(())
            }
            Constraint::Connected { view, .. } => {
                let v = view.as_mut().ok_or("ConnectedConstraint not initialised")?;
                v.update(w);
                let (articulation, component, components) = articulation_points(&v.neighbours, &v.could, &v.must);
                if components > 1 {
                    w.status = CONTRADICTION;
                    return Ok(());
                }
                for node in 0..v.must.len() {
                    if articulation[node] && !v.must[node] {
                        v.select_path(w, node);
                    }
                }
                if components > 0 {
                    for node in 0..v.must.len() {
                        if component[node].is_none() && v.could[node] {
                            v.ban_relevant(w, node);
                        }
                    }
                }
                Ok(())
            }
            Constraint::Parity { odd, view, .. } => {
                let (odd_set, odd_tracker) = odd.as_ref().ok_or("ParityConstraint not initialised")?;
                let v = view.as_ref().ok_or("ParityConstraint not initialised")?;
                let mut visited = vec![false; w.n];
                for i in 0..w.n {
                    if w.contains(i) {
                        parity_visit(w, v, odd_set, *odd_tracker, i, &mut visited);
                    }
                }
                Ok(())
            }
        }
    }
}

/// ParityConstraint.VisitIndex.
fn parity_visit(w: &mut Wfc, v: &PathView, odd: &TileSet, odd_tracker: Option<usize>, index: usize, visited: &mut [bool]) {
    if visited[index] {
        return;
    }
    let mut path_count = 0;
    let mut unset: Option<(usize, Option<usize>)> = None;
    let mut multiple = false;
    let mut stack = vec![index];
    while let Some(i) = stack.pop() {
        if visited[i] {
            continue;
        }
        visited[i] = true;
        let q = odd_tracker.map_or(-1, |t| w.quadstate(t, i));
        if q == 0 {
            if unset.is_some() {
                multiple = true;
            } else {
                unset = Some((i, None));
            }
        }
        if q == 1 {
            path_count += 1;
        }
        for d in 0..6 {
            let q = v.exit(d).map_or(-1, |e| w.quadstate(e.2, i));
            if q == 1 {
                path_count += 1;
            }
            if q == 0 {
                match w.try_move(i, d) {
                    Some(j) => {
                        if !visited[j] {
                            stack.push(j);
                        }
                    }
                    None => {
                        if unset.is_some() {
                            multiple = true;
                        } else {
                            unset = Some((i, Some(d)));
                        }
                    }
                }
            }
        }
    }
    if multiple {
        return;
    }
    match unset {
        Some((i, None)) => {
            if path_count % 2 == 0 {
                w.ban(i, odd);
            } else {
                w.select(i, odd);
            }
        }
        Some((i, Some(d))) => {
            let set = &v.exit(d).expect("an exit with a tracker").1;
            if path_count % 2 == 0 {
                w.ban(i, set);
            } else {
                w.select(i, set);
            }
        }
        None => {
            if path_count % 2 != 0 {
                w.status = CONTRADICTION;
            }
        }
    }
}

// ---- Tessera: generation ---------------------------------------------------------------------

/// A TesseraTileBase in the scene (TesseraInitialConstraintBuilder): its faces and cells, and
/// its matrix in the generator's space (generator.worldToLocalMatrix * tile.localToWorldMatrix).
pub struct SceneTile {
    pub faces: Vec<Face>,
    pub offsets: Vec<V3>,
    pub center: [f32; 3],
    pub matrix: Matrix,
}

/// A TesseraTileInstance: the tile and its pose in the generator's space.
#[derive(Debug, Clone)]
pub struct Instance {
    pub tile: usize,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
}

/// TesseraInitialConstraint: a scene tile placed on the generator's grid.
struct Placed {
    faces: Vec<Face>,
    offsets: Vec<V3>,
    cell: V3,
    rot: Rot,
}

fn translate(v: [f32; 3]) -> Matrix {
    trs(v, [0.0, 0.0, 0.0, 1.0], [1.0; 3])
}

fn scale(v: [f32; 3]) -> Matrix {
    trs([0.0; 3], [0.0, 0.0, 0.0, 1.0], v)
}

/// Matrix4x4.inverse.
pub fn inverse(m: &Matrix) -> Matrix {
    let a: [f64; 16] = std::array::from_fn(|k| m[k / 4][k % 4] as f64);
    let mut inv = [0.0f64; 16];
    inv[0] = a[5] * a[10] * a[15] - a[5] * a[11] * a[14] - a[9] * a[6] * a[15] + a[9] * a[7] * a[14] + a[13] * a[6] * a[11] - a[13] * a[7] * a[10];
    inv[4] = -a[4] * a[10] * a[15] + a[4] * a[11] * a[14] + a[8] * a[6] * a[15] - a[8] * a[7] * a[14] - a[12] * a[6] * a[11] + a[12] * a[7] * a[10];
    inv[8] = a[4] * a[9] * a[15] - a[4] * a[11] * a[13] - a[8] * a[5] * a[15] + a[8] * a[7] * a[13] + a[12] * a[5] * a[11] - a[12] * a[7] * a[9];
    inv[12] = -a[4] * a[9] * a[14] + a[4] * a[10] * a[13] + a[8] * a[5] * a[14] - a[8] * a[6] * a[13] - a[12] * a[5] * a[10] + a[12] * a[6] * a[9];
    inv[1] = -a[1] * a[10] * a[15] + a[1] * a[11] * a[14] + a[9] * a[2] * a[15] - a[9] * a[3] * a[14] - a[13] * a[2] * a[11] + a[13] * a[3] * a[10];
    inv[5] = a[0] * a[10] * a[15] - a[0] * a[11] * a[14] - a[8] * a[2] * a[15] + a[8] * a[3] * a[14] + a[12] * a[2] * a[11] - a[12] * a[3] * a[10];
    inv[9] = -a[0] * a[9] * a[15] + a[0] * a[11] * a[13] + a[8] * a[1] * a[15] - a[8] * a[3] * a[13] - a[12] * a[1] * a[11] + a[12] * a[3] * a[9];
    inv[13] = a[0] * a[9] * a[14] - a[0] * a[10] * a[13] - a[8] * a[1] * a[14] + a[8] * a[2] * a[13] + a[12] * a[1] * a[10] - a[12] * a[2] * a[9];
    inv[2] = a[1] * a[6] * a[15] - a[1] * a[7] * a[14] - a[5] * a[2] * a[15] + a[5] * a[3] * a[14] + a[13] * a[2] * a[7] - a[13] * a[3] * a[6];
    inv[6] = -a[0] * a[6] * a[15] + a[0] * a[7] * a[14] + a[4] * a[2] * a[15] - a[4] * a[3] * a[14] - a[12] * a[2] * a[7] + a[12] * a[3] * a[6];
    inv[10] = a[0] * a[5] * a[15] - a[0] * a[7] * a[13] - a[4] * a[1] * a[15] + a[4] * a[3] * a[13] + a[12] * a[1] * a[7] - a[12] * a[3] * a[5];
    inv[14] = -a[0] * a[5] * a[14] + a[0] * a[6] * a[13] + a[4] * a[1] * a[14] - a[4] * a[2] * a[13] - a[12] * a[1] * a[6] + a[12] * a[2] * a[5];
    inv[3] = -a[1] * a[6] * a[11] + a[1] * a[7] * a[10] + a[5] * a[2] * a[11] - a[5] * a[3] * a[10] - a[9] * a[2] * a[7] + a[9] * a[3] * a[6];
    inv[7] = a[0] * a[6] * a[11] - a[0] * a[7] * a[10] - a[4] * a[2] * a[11] + a[4] * a[3] * a[10] + a[8] * a[2] * a[7] - a[8] * a[3] * a[6];
    inv[11] = -a[0] * a[5] * a[11] + a[0] * a[7] * a[9] + a[4] * a[1] * a[11] - a[4] * a[3] * a[9] - a[8] * a[1] * a[7] + a[8] * a[3] * a[5];
    inv[15] = a[0] * a[5] * a[10] - a[0] * a[6] * a[9] - a[4] * a[1] * a[10] + a[4] * a[2] * a[9] + a[8] * a[1] * a[6] - a[8] * a[2] * a[5];
    let det = a[0] * inv[0] + a[1] * inv[4] + a[2] * inv[8] + a[3] * inv[12];
    std::array::from_fn(|i| std::array::from_fn(|j| (inv[i * 4 + j] / det) as f32))
}

fn column(m: &Matrix, j: usize) -> [f32; 3] {
    [m[0][j], m[1][j], m[2][j]]
}

fn fdot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Vector3.ProjectOnPlane.
fn project(v: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let sq = fdot(n, n);
    if sq < f32::EPSILON {
        return v;
    }
    let d = fdot(v, n);
    std::array::from_fn(|i| v[i] - n[i] * d / sq)
}

fn magnitude(v: [f32; 3]) -> f32 {
    (fdot(v, v) as f64).sqrt() as f32
}

/// Tessera.TRS(Matrix4x4): position, rotation and (signed x) scale.
fn decompose(m: &Matrix) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let position = [m[0][3], m[1][3], m[2][3]];
    let mut r = column(m, 0);
    let mut u = project(column(m, 1), r);
    let mut f = project(project(column(m, 2), r), u);
    let c = [r[1] * u[2] - r[2] * u[1], r[2] * u[0] - r[0] * u[2], r[0] * u[1] - r[1] * u[0]];
    let flip = fdot(c, f) < 0.0;
    let s = [if flip { -1.0 } else { 1.0 } * magnitude(r), magnitude(u), magnitude(f)];
    r = r.map(|x| x / s[0]);
    u = u.map(|x| x / s[1]);
    f = f.map(|x| x / s[2]);
    let t = r[0] + u[1] + f[2];
    let q = if t > 0.0 {
        let s = ((t + 1.0) as f64).sqrt() as f32 * 2.0;
        [(u[2] - f[1]) / s, (f[0] - r[2]) / s, (r[1] - u[0]) / s, 0.25 * s]
    } else if r[0] > u[1] && r[0] > f[2] {
        let s = ((1.0 + r[0] - u[1] - f[2]) as f64).sqrt() as f32 * 2.0;
        [0.25 * s, (u[0] + r[1]) / s, (f[0] + r[2]) / s, (u[2] - f[1]) / s]
    } else if u[1] > f[2] {
        let s = ((1.0 + u[1] - r[0] - f[2]) as f64).sqrt() as f32 * 2.0;
        [(u[0] + r[1]) / s, 0.25 * s, (f[1] + u[2]) / s, (f[0] - r[2]) / s]
    } else {
        let s = ((1.0 + f[2] - r[0] - u[1]) as f64).sqrt() as f32 * 2.0;
        [(f[0] + r[2]) / s, (f[1] + u[2]) / s, 0.25 * s, (r[1] - u[0]) / s]
    };
    (position, q, s)
}

/// TRS * TRS (through Matrix4x4.TRS and back).
fn trs_mul(a: ([f32; 3], [f32; 4], [f32; 3]), b: ([f32; 3], [f32; 4], [f32; 3])) -> ([f32; 3], [f32; 4], [f32; 3]) {
    decompose(&matrix_mul(&trs(a.0, a.1, a.2), &trs(b.0, b.1, b.2)))
}

fn rot_float_matrix(r: Rot) -> Matrix {
    let m = rot_matrix(r);
    let mut out = [[0.0; 4]; 4];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = m[i][j] as f32;
        }
    }
    out[3][3] = 1.0;
    out
}

/// The generator's grid: a CubeGrid of its size and cell size moved to its origin.
struct Grid {
    bound: Bound,
    size: [usize; 3],
    cell_size: [f32; 3],
    /// TransformModifier's translation: origin - cellSize / 2.
    shift: [f32; 3],
}

impl Grid {
    fn new(g: &Generator) -> Grid {
        // TesseraGenerator.origin: center - CubeGeometryUtils.GetCellCenter(size - 1) / 2.
        let origin: [f32; 3] = std::array::from_fn(|i| g.center[i] - ((g.size[i] - 1) as f32 * g.cell_size[i]) / 2.0);
        Grid { bound: Bound { min: [0; 3], max: g.size }, size: g.size.map(|s| s as usize), cell_size: g.cell_size, shift: std::array::from_fn(|i| origin[i] - g.cell_size[i] / 2.0) }
    }

    /// CubeBound enumeration: x, then y, then z innermost.
    fn cells(&self) -> Vec<V3> {
        let mut out = Vec::new();
        for x in 0..self.size[0] as i32 {
            for y in 0..self.size[1] as i32 {
                for z in 0..self.size[2] as i32 {
                    out.push([x, y, z]);
                }
            }
        }
        out
    }

    fn index(&self, c: V3) -> usize {
        c[0] as usize + c[1] as usize * self.size[0] + c[2] as usize * self.size[0] * self.size[1]
    }

    /// SylvesExtensions.FindCell on the unbounded grid: the cell and rotation of a tile.
    fn find_cell(&self, center: [f32; 3], m: &Matrix) -> Option<(V3, Rot)> {
        let m = matrix_mul(&translate(self.shift.map(|v| -v)), &matrix_mul(m, &translate(center)));
        // CubeRotation.FromMatrix: each basis vector rotated, normalised and rounded.
        let mut cols = [[0i32; 3]; 3];
        for (j, col) in cols.iter_mut().enumerate() {
            let v = column(&m, j);
            let len = magnitude(v);
            let n = if len > 1e-5 { v.map(|x| x / len) } else { [0.0; 3] };
            *col = n.map(|x| x.round_ties_even() as i32);
            if col.iter().map(|x| x.abs()).sum::<i32>() != 1 {
                return None;
            }
        }
        let rot = from_matrix_simple(std::array::from_fn(|i| std::array::from_fn(|j| cols[j][i])));
        let p = [m[0][3], m[1][3], m[2][3]];
        Some((std::array::from_fn(|i| (p[i] / self.cell_size[i]).floor() as i32), rot))
    }

    /// TransformModifier.GetTRS: the cell's center, unrotated, scaled by the cell size.
    fn cell_trs(&self, c: V3) -> ([f32; 3], [f32; 4], [f32; 3]) {
        let center: [f32; 3] = std::array::from_fn(|i| (c[i] as f32 + 0.5) * self.cell_size[i]);
        decompose(&matrix_mul(&translate(self.shift), &trs(center, [0.0, 0.0, 0.0, 1.0], self.cell_size)))
    }
}

/// TesseraGenerator.Generate with this seed: the tiles in TesseraCompletion.tileInstances
/// order, or None when all its retries contradicted.
pub fn generate(model: &Model, scene: &[SceneTile], seed: i32) -> Result<Option<Vec<Instance>>, String> {
    let g = &model.generator;
    let grid = Grid::new(g);
    let mut rng = XoRo::new(seed as i64);
    // SearchInitialConstraints: the scene's tiles on the grid (FindCell of their center).
    let mut placed = Vec::new();
    if g.search_initial_constraints {
        for t in scene {
            if let Some((cell, rot)) = grid.find_cell(t.center, &t.matrix) {
                placed.push(Placed { faces: t.faces.clone(), offsets: t.offsets.clone(), cell, rot });
            }
        }
    }
    // TesseraInitialConstraintHelper.GetMask: the scene tiles' cells are not generated.
    let mut mask = vec![true; grid.size.iter().product()];
    for p in &placed {
        for o in &p.offsets {
            if let Some((c, _)) = parallel_transport([0; 3], *o, Some(&grid.bound), p.cell, p.rot)? {
                mask[grid.index(c)] = false;
            }
        }
    }
    let lookup: IndexMap<u8, Vec<ModelTile>> = model.internal.iter().fold(IndexMap::new(), |mut l, a| {
        l.entry(a.dir).or_insert_with(Vec::new).push(a.src);
        l
    });
    for _ in 0..g.retries {
        let mut w = Wfc::new(model, grid.size, Some(mask.clone()), &mut rng);
        let mut constraints = make_constraints(model)?;
        if w.status != CONTRADICTION {
            // WavePropagator.InitConstraints.
            for c in constraints.iter_mut() {
                c.init(&mut w, &mut rng)?;
                if w.status != UNDECIDED {
                    break;
                }
                w.propagate();
                if w.status != UNDECIDED {
                    break;
                }
            }
        }
        apply_initial_constraints(model, &grid, &placed, &mut w)?;
        // BanBigTiles: no part of a big tile across the grid's edge.
        for c in grid.cells() {
            let i = grid.index(c);
            if !w.contains(i) {
                continue;
            }
            for dir in 0..6u8 {
                if grid.bound.contains(add(c, forward(dir))) {
                    continue;
                }
                for t in lookup.get(&dir).into_iter().flatten() {
                    let set = model.tile_set([*t]);
                    w.ban(i, &set);
                }
            }
        }
        while w.status == UNDECIDED {
            w.step(&mut constraints, &mut rng)?;
        }
        if w.status == CONTRADICTION {
            continue;
        }
        return instances(model, &grid, &w).map(Some);
    }
    Ok(None)
}

/// TesseraGenerator.GetTileConstraints in component order: PathConstraint (connected, then
/// parity), then the count constraints.
fn make_constraints(model: &Model) -> Result<Vec<Constraint>, String> {
    let mut out = Vec::new();
    for c in &model.generator.constraints {
        match c {
            ConstraintDef::Count { tiles, comparison, count, eager } => {
                let set = model.tile_set(model.first_offsets(tiles)?);
                out.push(Constraint::Count { set, tracker: 0, comparison: *comparison, count: *count, eager: *eager });
            }
            ConstraintDef::Path { colors, parity } => {
                let exits = model.exits(colors);
                out.push(Constraint::Connected { exits: exits.clone(), view: None });
                if *parity {
                    out.push(Constraint::Parity { exits, odd: None, view: None });
                }
            }
        }
    }
    Ok(out)
}

/// TesseraGeneratorHelper.ApplyInitialConstraintsAndSkybox.
fn apply_initial_constraints(model: &Model, grid: &Grid, placed: &[Placed], w: &mut Wfc) -> Result<(), String> {
    let mut constrained: HashSet<(V3, u8)> = HashSet::new();
    let mut face = |w: &mut Wfc, p2: V3, inverse: u8, fd: &FaceDetails| {
        if !grid.bound.contains(p2) || !w.contains(grid.index(p2)) || !constrained.insert((p2, inverse)) {
            return;
        }
        let tiles: Vec<ModelTile> = model.by_dir[inverse as usize].iter().filter(|(f, _)| model.generator.palette.matches(fd, f)).map(|(_, t)| *t).collect();
        let set = model.tile_set(tiles);
        w.select(grid.index(p2), &set);
    };
    for p in placed {
        for f in &p.faces {
            if let Some((c, _)) = parallel_transport([0; 3], f.offset, None, p.cell, p.rot)? {
                let (d, fd) = rotate_by(f.dir, &f.details, p.rot);
                let n = add(c, forward(d));
                if grid.bound.contains(n) {
                    face(w, n, d ^ 1, &fd);
                }
            }
        }
    }
    if let Some(sky) = &model.sky_box {
        for c in grid.cells() {
            for dir in 0..6u8 {
                if !grid.bound.contains(add(c, forward(dir))) {
                    face(w, c, dir, &sky[dir as usize]);
                }
            }
        }
    }
    Ok(())
}

/// SylvesDeBroglieUtils.ToTileDictionary and SylvesTesseraTilemapConversions.ToTileInstances.
fn instances(model: &Model, grid: &Grid, w: &Wfc) -> Result<Vec<Instance>, String> {
    let tiles: Vec<ModelTile> = model.patterns.keys().copied().collect();
    let mut filled: HashSet<V3> = HashSet::new();
    let mut out = Vec::new();
    for i in 0..w.n {
        if !w.contains(i) {
            continue;
        }
        let p = w.decided(i).ok_or("a cell left undecided")?;
        let m = tiles[p];
        let cell = w.coord(i);
        if filled.contains(&cell) {
            continue;
        }
        let t = &model.tiles[m.tile];
        // ToTileInstance: the cells of the whole tile, and its pose.
        for o in &t.offsets {
            let (c, _) = parallel_transport(m.offset, *o, Some(&grid.bound), cell, m.rot)?.ok_or(format!("BigTile {} is not fully contained in topology.", t.name))?;
            filled.insert(c);
        }
        let size_transform = matrix_mul(&matrix_mul(&translate(t.center), &scale(t.cell_size)), &translate(m.offset.map(|v| v as f32)));
        let local = trs_mul(trs_mul(grid.cell_trs(cell), decompose(&rot_float_matrix(m.rot))), decompose(&inverse(&size_transform)));
        out.push(Instance { tile: m.tile, position: local.0, rotation: local.1 });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotations_compose_invert_and_move_dirs_as_sylves() {
        let all = cube_rotations(true);
        assert_eq!(all.len(), 48);
        for &a in &all {
            assert_eq!(rot_mul(a, rot_invert(a)), IDENTITY);
            for &b in &all {
                for v in [[1, 2, 3], [-3, 0, 5]] {
                    assert_eq!(rot_apply(rot_mul(a, b), v), rot_apply(a, rot_apply(b, v)));
                }
            }
            // A parallel transport through a regular cube grid moves by the rotated offset and
            // keeps the rotation.
            assert_eq!(parallel_transport([0; 3], [2, -1, 3], None, [5, 5, 5], a).unwrap(), Some((add([5, 5, 5], rot_apply(a, [2, -1, 3])), a)));
        }
    }
}
